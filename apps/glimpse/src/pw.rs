//! The `pw` provider: rows from the Nixe Passwords daemon and what Enter does to them.
//! Rows carry an item's id and metadata, never a secret; copies happen in the daemon,
//! which owns the clipboard (and clears it), so Glimpse can close at once.

use gtk::{gio, glib, prelude::*};
use nixe_glimpse::passwords::{Query, Special, Verb};
use nixe_passwords_proto::{Error as Code, client, state};

#[derive(Clone, Debug, PartialEq)]
pub enum Pw {
    Item {
        id: String,
        kind: String,
        username: String,
        host: String,
        has_totp: bool,
        verb: Verb,
    },
    Generate,
    Lock,
    /// The vault is locked: Enter opens Passwords on its unlock page.
    Unlock,
    /// No account yet: Enter opens Passwords to sign in.
    SignIn,
    /// Passwords isn't installed or its service won't start.
    Unavailable,
    /// Guidance in place of results (nothing typed yet, nothing matched); Enter does nothing.
    Help,
}

impl Pw {
    pub fn key(&self) -> String {
        match self {
            Pw::Item { id, .. } => format!("pw:{id}"),
            other => format!("pw:{other:?}"),
        }
    }

    pub fn icon(&self) -> &'static str {
        match self {
            Pw::Item { kind, .. } => match kind.as_str() {
                "note" => "text-x-generic-symbolic",
                "identity" => "avatar-default-symbolic",
                _ => "dialog-password-symbolic",
            },
            Pw::Generate => "media-playlist-shuffle-symbolic",
            Pw::Lock | Pw::Unlock => "system-lock-screen-symbolic",
            Pw::SignIn => "dialog-password-symbolic",
            Pw::Unavailable => "dialog-warning-symbolic",
            Pw::Help => "system-search-symbolic",
        }
    }

    /// The status bar's keys for this row.
    pub fn hints(&self) -> Vec<(&'static [&'static str], &'static str)> {
        match self {
            Pw::Item { verb, has_totp, kind, .. } => {
                let mut list = vec![(&["Enter"][..], verb.label())];
                if kind == "login" {
                    if *verb != Verb::Password {
                        list.push((&["Ctrl", "Enter"][..], "Password"));
                    }
                    if *has_totp && *verb != Verb::Totp {
                        list.push((&["Alt", "Enter"][..], "Code"));
                    }
                }
                list
            }
            Pw::Generate => vec![(&["Enter"][..], "Generate and copy")],
            Pw::Lock => vec![(&["Enter"][..], "Lock")],
            Pw::Unlock => vec![(&["Enter"][..], "Unlock in Passwords")],
            Pw::SignIn => vec![(&["Enter"][..], "Open Passwords")],
            Pw::Unavailable | Pw::Help => vec![],
        }
    }

    /// The preview pane's rows.
    pub fn details(&self) -> Vec<(String, String)> {
        match self {
            Pw::Item { kind, username, host, has_totp, .. } => {
                let mut d = vec![("Kind".to_string(), kind_name(kind).to_string())];
                if !username.is_empty() {
                    d.push(("Username".into(), username.clone()));
                }
                if !host.is_empty() {
                    d.push(("Website".into(), host.clone()));
                }
                if kind == "login" {
                    d.push(("One-time code".into(), if *has_totp { "Yes" } else { "No" }.into()));
                    if !host.is_empty() {
                        // The status bar has no room for this one.
                        d.push(("Open the site".into(), "Ctrl+O".into()));
                    }
                }
                d
            }
            _ => vec![],
        }
    }
}

fn kind_name(kind: &str) -> &'static str {
    match kind {
        "login" => "Login",
        "note" => "Secure note",
        "card" => "Card",
        "identity" => "Identity",
        "ssh" => "SSH key",
        _ => "Item",
    }
}

/// One row: title, second line, what it is.
pub type Row = (String, String, Pw);

/// The rows for a `pw` query, and a status message when there's something to say.
pub async fn lookup(q: &Query) -> (Vec<Row>, String) {
    let st = match client::status().await {
        Ok((st, _, _)) => st,
        Err(e) => {
            let title = if client::installed() { "Passwords isn't answering" } else { "Passwords isn't installed" };
            return (vec![(title.into(), e.to_string(), Pw::Unavailable)], String::new());
        }
    };
    if st == state::LOGGED_OUT {
        return (vec![("Sign in to Vaultwarden".into(), "Opens Passwords".into(), Pw::SignIn)], String::new());
    }
    if st == state::LOCKED {
        let rows = match q.special {
            // Already locked: nothing to do, but say so rather than offering to unlock.
            Some(Special::Lock) => return (vec![], "The vault is already locked".into()),
            _ => vec![("Vault locked".into(), "Enter to unlock it in Passwords".into(), Pw::Unlock)],
        };
        return (rows, String::new());
    }
    let mut rows = Vec::new();
    match q.special {
        Some(Special::Generate) => rows.push((
            "Generate a password".to_string(),
            "Copies a new one, made with Passwords' generator settings".to_string(),
            Pw::Generate,
        )),
        Some(Special::Lock) => rows.push(("Lock the vault".to_string(), String::new(), Pw::Lock)),
        None => {}
    }
    match client::search(&q.terms, 10).await {
        Ok((_origin, items)) => {
            for (id, name, username, host, has_totp, kind) in items {
                let second =
                    [username.as_str(), host.as_str()].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" · ");
                rows.push((name, second, Pw::Item { id, kind, username, host, has_totp, verb: q.verb }));
            }
        }
        Err(e) if e.is(Code::Denied) => return (rows, "Glimpse is turned off in Passwords' settings".into()),
        Err(e) if e.is(Code::Locked) => {
            return (vec![("Vault locked".into(), "Enter to unlock it in Passwords".into(), Pw::Unlock)], String::new());
        }
        Err(e) => return (rows, e.to_string()),
    }
    if rows.is_empty() {
        let help = if q.terms.is_empty() {
            ("Type a name after pw".to_string(), "pw github · pw github pass · pw generate · pw lock".to_string())
        } else {
            (format!("Nothing in the vault matches “{}”", q.terms), "Try fewer or other words".to_string())
        };
        rows.push((help.0, help.1, Pw::Help));
    }
    (rows, String::new())
}

/// Acts on a row: `verb` overrides the row's own (Ctrl+Enter, Alt+Enter, Ctrl+O, Ctrl+C).
/// The window hides once the daemon has done it; errors stay in the status bar.
pub fn activate(pw: &Pw, verb: Option<Verb>, window: &gtk::ApplicationWindow, status: &gtk::Label) {
    let (pw, window, status) = (pw.clone(), window.clone(), status.clone());
    glib::spawn_future_local(async move {
        let done = match &pw {
            Pw::Item { id, verb: own, kind, has_totp, .. } => {
                let verb = verb.unwrap_or(*own);
                if verb == Verb::Totp && !has_totp {
                    Err("This item has no one-time code".to_string())
                } else if kind != "login" && verb != Verb::Username {
                    Err(format!("This is a {}: open it in Passwords", kind_name(kind).to_lowercase()))
                } else {
                    match verb.field() {
                        Some(field) => client::copy(id, field).await.map(|_| ()).map_err(message),
                        None => match client::text("OpenUrl", Some((id.as_str(),).to_variant())).await {
                            Ok(url) => gio::AppInfo::launch_default_for_uri(&url, None::<&gio::AppLaunchContext>)
                                .map_err(|e| e.to_string()),
                            Err(e) => Err(message(e)),
                        },
                    }
                }
            }
            Pw::Generate => client::call("Generate", None).await.map(|_| ()).map_err(message),
            Pw::Lock => client::call("Lock", None).await.map(|_| ()).map_err(message),
            Pw::Unlock | Pw::SignIn => std::process::Command::new("nixe-passwords")
                .arg("--unlock")
                .spawn()
                .map(|_| ())
                .map_err(|_| "Couldn't open Passwords".to_string()),
            Pw::Unavailable => Err("Install Passwords with: nixe install passwords".to_string()),
            Pw::Help => return,
        };
        match done {
            Ok(()) => window.set_visible(false),
            Err(e) => status.set_text(&e),
        }
    });
}

fn message(e: client::Fail) -> String {
    match e.code {
        Some(Code::Reprompt) => "This item asks for the master password: open it in Passwords".into(),
        Some(Code::NoField) => "This item doesn't have that".into(),
        Some(Code::NoUrl) => "This item has no web address".into(),
        Some(Code::RateLimited) => "Too many copies in a row; wait a minute".into(),
        Some(Code::Clipboard) => "Couldn't reach the clipboard".into(),
        _ => e.to_string(),
    }
}
