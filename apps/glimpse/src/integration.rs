use crate::{
    config::{Paths, atomic_write},
    process,
};
use anyhow::{Context, Result, bail};
use std::{fs, path::Path, process::Command, time::Duration};
const START: &str = "-- BEGIN NYX GLIMPSE";
const END: &str = "-- END NYX GLIMPSE";
pub fn block(shortcuts: bool) -> String {
    block_keys(shortcuts, "SUPER + SPACE", "CTRL + SPACE")
}
pub fn block_keys(shortcuts: bool, search_key: &str, menu_key: &str) -> String {
    let mut s = format!("{START}\n-- Owned by Glimpse. Remove through Glimpse Settings.\n");
    // Nothing else: the launcher is a layer-shell overlay that places itself and hides on a
    // click beside its panel (`ui::present`), so no window rule and no mouse bindings are needed.
    if shortcuts {
        s.push_str("o.rebind(\"SUPER + SPACE\", \"Glimpse\", { launch = \"nyx-glimpse --toggle\" })\no.rebind(\"CTRL + SPACE\", \"Omarchy menu\", { menu = \"root\" })\n");
    }
    s = s.replace("SUPER + SPACE", search_key).replace("CTRL + SPACE", menu_key);
    s.push_str(END);
    s.push('\n');
    s
}
pub fn edit(text: &str, old: Option<&str>, new: Option<&str>) -> Result<String> {
    let starts = text.matches(START).count();
    let ends = text.matches(END).count();
    if starts == 0 && ends == 0 {
        if old.is_some() {
            bail!("owned block was removed externally");
        }
        return Ok(match new {
            Some(s) => format!("{}{}{}", text, if text.ends_with('\n') || text.is_empty() { "" } else { "\n" }, s),
            None => text.into(),
        });
    }
    if starts != 1 || ends != 1 {
        bail!("malformed Glimpse ownership markers");
    }
    let start = text.find(START).unwrap();
    let end = text.find(END).unwrap() + END.len();
    if start >= end {
        bail!("malformed ownership block");
    }
    let end = if text.as_bytes().get(end) == Some(&b'\n') { end + 1 } else { end };
    let expected = old.context("unrecognized owned block; refusing to overwrite")?;
    if &text[start..end] != expected {
        bail!("Glimpse block was edited; refusing unsafe removal");
    }
    Ok(format!("{}{}{}", &text[..start], new.unwrap_or(""), &text[end..]))
}
fn check() -> Result<()> {
    let mut c = Command::new("hyprctl");
    c.arg("reload");
    process::capture(c, vec![], Duration::from_secs(5), 65536)?;
    let mut c = Command::new("hyprctl");
    c.arg("configerrors");
    let out = process::capture(c, vec![], Duration::from_secs(5), 65536)?;
    let s = String::from_utf8(out)?;
    if !s.trim().is_empty() && !s.to_lowercase().contains("no errors") {
        bail!("Hyprland reports: {s}");
    }
    Ok(())
}
pub fn apply_at(paths: &Paths, file: &Path, shortcuts: Option<bool>, live: bool) -> Result<()> {
    let original = fs::read_to_string(file)?;
    let owned = paths.state.join("hypr-block.lua");
    let old = fs::read_to_string(&owned).ok();
    let preferences = paths.load()?;
    let new = shortcuts.map(|enabled| block_keys(enabled, &preferences.search_key, &preferences.menu_key));
    let changed = edit(&original, old.as_deref(), new.as_deref())?;
    if original == changed {
        return Ok(());
    }
    let backup = paths
        .state
        .join(format!("hyprland-{}.lua", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos()));
    atomic_write(&backup, original.as_bytes())?;
    // Preserve existing file permissions; atomic_write must not tighten the containing hypr directory.
    fn write_user(file: &Path, bytes: &[u8]) -> Result<()> {
        let mode = fs::metadata(file)?.permissions();
        let tmp = file.with_extension(format!("glimpse-{}", std::process::id()));
        fs::write(&tmp, bytes)?;
        fs::set_permissions(&tmp, mode)?;
        fs::rename(tmp, file)?;
        Ok(())
    }
    write_user(file, changed.as_bytes())?;
    if live && let Err(e) = check() {
        write_user(file, original.as_bytes())?;
        let _ = check();
        return Err(e.context("rolled back Glimpse configuration"));
    }
    if let Some(new) = new {
        atomic_write(&owned, new.as_bytes())?;
    } else if owned.exists() {
        fs::remove_file(owned)?;
    }
    Ok(())
}
pub fn apply(paths: &Paths, shortcuts: Option<bool>) -> Result<()> {
    let parent = paths.config.parent().context("XDG config parent")?;
    let file = parent.join("hypr/hyprland.lua");
    if !file.exists() && shortcuts.is_none() && !paths.state.join("hypr-block.lua").exists() {
        return Ok(());
    }
    apply_at(paths, &file, shortcuts, true)
}
fn service_unit(paths: &Paths) -> Result<()> {
    let folder = paths.config.parent().context("XDG config parent")?.join("systemd/user");
    fs::create_dir_all(&folder)?;
    let bin = std::env::current_exe()?.parent().context("binary directory")?.join("nyx-glimpsed");
    if !bin.exists() {
        bail!("nyx-glimpsed is not installed next to Glimpse");
    }
    let escaped = bin.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"").replace('%', "%%");
    let unit = format!(
        "[Unit]\nDescription=Glimpse local search index\nAfter=graphical-session.target\nPartOf=graphical-session.target\n\n[Service]\nExecStart=\"{escaped}\"\nRestart=on-failure\nUMask=0077\nNice=10\n\n[Install]\nWantedBy=default.target\n"
    );
    fs::write(folder.join("nyx-glimpsed.service"), unit)?;
    let mut c = Command::new("systemctl");
    c.args(["--user", "daemon-reload"]);
    process::capture(c, vec![], Duration::from_secs(10), 65536)?;
    Ok(())
}
pub fn user_service(paths: &Paths, enabled: bool) -> Result<()> {
    service_unit(paths)?;
    let mut c = Command::new("systemctl");
    c.args(["--user", if enabled { "enable" } else { "disable" }, "--now", "nyx-glimpsed.service"]);
    process::capture(c, vec![], Duration::from_secs(10), 65536)?;
    Ok(())
}
pub fn start_indexing(paths: &Paths) -> Result<()> {
    // Starting the indexer does not change whether it starts at login.
    service_unit(paths)?;
    let mut c = Command::new("systemctl");
    c.args(["--user", "start", "nyx-glimpsed.service"]);
    process::capture(c, vec![], Duration::from_secs(10), 65536)?;
    Ok(())
}
