//! Host side of the Mail, Calendar and Reminders plugins. Each reads one
//! database its app keeps for outside readers, mounted read-only into the
//! sandbox (never the rest of the app's folder: no message store, no
//! credentials). The sandbox can't start apps or open links, so the results'
//! actions run here, from a small JSON token checked field by field:
//!
//! - Mail: `{"app": "mail", "op": "open", "message": ID}` → `nyx-mail --message ID`
//! - Calendar: `{"app": "calendar", "op": "open", "day": "YYYY-MM-DD"}` →
//!   `nyx-calendar --date DAY`; `{"op": "join", "url": "https://…"}` opens a
//!   meeting link, only on known meeting hosts
//! - Reminders: `{"op": "open", "list": UID}` → `nyx-reminders --section list:UID`;
//!   `{"op": "add", "text": …}` → `nyx-reminders --add TEXT`
//!
//! Tokens carry `"api_version": 1`, and one plugin can only act for its own app.
//! Programs get their arguments directly, never through a shell.

use crate::config::Paths;
use crate::plugins;
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

struct Library {
    plugin: &'static str,
    /// Under the data home.
    file: &'static str,
    mount: &'static str,
    env: &'static str,
}

const LIBRARIES: [Library; 3] = [
    Library { plugin: "nyx-mail", file: "nyx-mail/mail.sqlite", mount: "/mail-library/mail.sqlite", env: "NYX_MAIL_DB" },
    Library { plugin: "nyx-calendar", file: "nyx-calendar/agenda.sqlite", mount: "/calendar-library/agenda.sqlite", env: "NYX_CALENDAR_DB" },
    Library { plugin: "nyx-reminders", file: "nyx-reminders/tasks.sqlite", mount: "/reminders-library/tasks.sqlite", env: "NYX_REMINDERS_DB" },
];

/// Mounts the plugin's database (and Mail's write-ahead log beside it)
/// read-only. Returns the environment variable naming it inside the sandbox.
pub fn bind(cmd: &mut Command, plugin_id: &str, data_home: &Path) -> Option<(&'static str, &'static str)> {
    let lib = LIBRARIES.iter().find(|l| l.plugin == plugin_id)?;
    let file = data_home.join(lib.file);
    if !file.is_file() {
        return None;
    }
    cmd.arg("--ro-bind").arg(&file).arg(lib.mount);
    for suffix in ["-wal", "-shm"] {
        let side = PathBuf::from(format!("{}{suffix}", file.display()));
        if side.is_file() {
            cmd.arg("--ro-bind").arg(&side).arg(format!("{}{suffix}", lib.mount));
        }
    }
    Some((lib.env, lib.mount))
}

/// What a token asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum Launch {
    Program(&'static str, Vec<String>),
    Link(String),
}

fn day(text: &str) -> bool {
    let b = text.as_bytes();
    b.len() == 10
        && b.iter().enumerate().all(|(i, c)| if i == 4 || i == 7 { *c == b'-' } else { c.is_ascii_digit() })
}

fn list_uid(text: &str) -> bool {
    !text.is_empty() && text.len() <= 200 && text.chars().all(|c| c.is_ascii_alphanumeric() || "-_.@:".contains(c))
}

/// Meeting hosts a Join result may open.
fn meeting_link(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else { return false };
    if url.len() > 2048 || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return false;
    }
    let host = rest.split(['/', '?', '#']).next().unwrap_or("").to_ascii_lowercase();
    if host.contains('@') || host.contains(':') {
        return false;
    }
    ["meet.google.com", "zoom.us", "teams.microsoft.com", "teams.live.com", "meet.jit.si", "whereby.com"]
        .iter()
        .any(|h| host == *h || host.ends_with(&format!(".{h}")))
}

/// Checks a token from `plugin_id`; `None` when it isn't one of these apps'.
pub fn parse(plugin_id: &str, token: &str) -> Option<Result<Launch>> {
    let app = match plugin_id {
        "nyx-mail" => "mail",
        "nyx-calendar" => "calendar",
        "nyx-reminders" => "reminders",
        _ => return None,
    };
    Some((|| {
        if token.len() > 4096 {
            bail!("the action is too long");
        }
        let v: Value = serde_json::from_str(token).context("the action isn't understood")?;
        if v["api_version"] != 1 || v["app"] != app {
            bail!("the action isn't for this app");
        }
        let text = |key: &str| v[key].as_str().unwrap_or("").to_string();
        Ok(match (app, v["op"].as_str().unwrap_or("")) {
            ("mail", "open") => {
                let id = v["message"].as_i64().filter(|n| *n > 0).context("no message")?;
                Launch::Program("nyx-mail", vec!["--message".into(), id.to_string()])
            }
            ("calendar", "open") if day(&text("day")) => Launch::Program("nyx-calendar", vec!["--date".into(), text("day")]),
            ("calendar", "join") if meeting_link(&text("url")) => Launch::Link(text("url")),
            ("calendar", "join") => bail!("that isn't a meeting link Glimpse opens"),
            ("reminders", "open") if list_uid(&text("list")) => {
                Launch::Program("nyx-reminders", vec!["--section".into(), format!("list:{}", text("list"))])
            }
            ("reminders", "add") => {
                let words = text("text");
                if words.trim().is_empty() || words.chars().count() > 500 || words.chars().any(char::is_control) {
                    bail!("that reminder can't be added");
                }
                // A leading dash would read as an option.
                Launch::Program("nyx-reminders", vec!["--add".into(), words.trim_start_matches('-').trim().to_string()])
            }
            _ => bail!("unsupported action"),
        })
    })())
}

fn program(name: &str) -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        let local = PathBuf::from(home).join(".local/bin").join(name);
        if local.is_file() {
            return local;
        }
    }
    PathBuf::from(name)
}

/// Runs a result's action for one of these plugins; `None` for other plugins.
pub fn host_action(paths: &Paths, plugin_id: &str, token: &str) -> Option<Result<()>> {
    let launch = parse(plugin_id, token)?;
    Some((|| {
        let plugin = plugins::get(paths, plugin_id)?;
        if !plugin.grant.enabled || plugin.grant.version != plugin.manifest.version || plugin.grant.requested != plugin.manifest.permissions {
            bail!("this plugin was turned off or its permissions changed");
        }
        match launch? {
            Launch::Program(name, args) => {
                let exe = program(name);
                Command::new(&exe).args(args).spawn().with_context(|| format!("couldn't start {}", exe.display()))?;
            }
            Launch::Link(url) => {
                Command::new("xdg-open").arg(url).spawn().context("couldn't open the link")?;
            }
        }
        Ok(())
    })())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn launch(plugin: &str, token: &str) -> Result<Launch> {
        parse(plugin, token).expect("one of these plugins")
    }

    #[test]
    fn tokens_are_checked() {
        assert!(parse("nyx-news", "{}").is_none());
        assert_eq!(
            launch("nyx-mail", r#"{"api_version":1,"app":"mail","op":"open","message":42}"#).unwrap(),
            Launch::Program("nyx-mail", vec!["--message".into(), "42".into()])
        );
        // A plugin acts only for its own app.
        assert!(launch("nyx-mail", r#"{"api_version":1,"app":"reminders","op":"add","text":"x"}"#).is_err());
        assert!(launch("nyx-mail", r#"{"api_version":2,"app":"mail","op":"open","message":42}"#).is_err());
        assert!(launch("nyx-mail", r#"{"api_version":1,"app":"mail","op":"open","message":"42; rm"}"#).is_err());
        assert_eq!(
            launch("nyx-calendar", r#"{"api_version":1,"app":"calendar","op":"open","day":"2026-10-21"}"#).unwrap(),
            Launch::Program("nyx-calendar", vec!["--date".into(), "2026-10-21".into()])
        );
        assert!(launch("nyx-calendar", r#"{"api_version":1,"app":"calendar","op":"open","day":"--help"}"#).is_err());
        assert_eq!(
            launch("nyx-reminders", r#"{"api_version":1,"app":"reminders","op":"add","text":"--section x call Sam"}"#).unwrap(),
            Launch::Program("nyx-reminders", vec!["--add".into(), "section x call Sam".into()])
        );
        assert!(launch("nyx-reminders", r#"{"api_version":1,"app":"reminders","op":"open","list":"a/../b"}"#).is_err());
        assert!(launch("nyx-reminders", r#"{"api_version":1,"app":"reminders","op":"add","text":"a\nb"}"#).is_err());
    }

    #[test]
    fn only_meeting_links_open() {
        let join = |url: &str| launch("nyx-calendar", &serde_json::json!({"api_version":1,"app":"calendar","op":"join","url":url}).to_string());
        assert_eq!(join("https://meet.google.com/abc-defg-hij").unwrap(), Launch::Link("https://meet.google.com/abc-defg-hij".into()));
        assert!(join("https://acme.zoom.us/j/123").is_ok());
        assert!(join("http://meet.google.com/abc").is_err());
        assert!(join("https://evil.example/meet.google.com/").is_err());
        assert!(join("https://meet.google.com.evil.example/").is_err());
        assert!(join("https://user@meet.google.com/").is_err());
        assert!(join("https://notzoom.us/j/1").is_err());
        assert!(join("file:///etc/passwd").is_err());
    }

    #[test]
    fn mounts_only_the_database() {
        let home = std::env::temp_dir().join(format!("glimpse-pim-{}", std::process::id()));
        std::fs::create_dir_all(home.join("nyx-mail/store")).unwrap();
        std::fs::write(home.join("nyx-mail/mail.sqlite"), b"").unwrap();
        std::fs::write(home.join("nyx-mail/mail.sqlite-wal"), b"").unwrap();
        let mut cmd = Command::new("bwrap");
        assert_eq!(bind(&mut cmd, "nyx-mail", &home), Some(("NYX_MAIL_DB", "/mail-library/mail.sqlite")));
        let args: Vec<String> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args.iter().filter(|a| *a == "--ro-bind").count(), 2);
        assert!(!args.iter().any(|a| a.contains("store")));
        assert!(bind(&mut Command::new("bwrap"), "nyx-calendar", &home).is_none(), "no agenda yet");
        let _ = std::fs::remove_dir_all(home);
    }
}
