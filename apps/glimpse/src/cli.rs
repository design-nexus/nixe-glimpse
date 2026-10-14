use crate::{
    config::{self, Paths},
    index, integration, plugins, semantic, settings,
};
use anyhow::{Context, Result, bail};
use serde_json::json;
use std::{fs, path::Path, process::Command};
fn arg(a: &[String], i: usize) -> Result<&str> {
    a.get(i).map(String::as_str).context("missing command argument")
}
pub fn dispatch(a: &[String]) -> Result<bool> {
    let p = Paths::discover();
    let Some(op) = a.first().map(String::as_str) else {
        return Ok(false);
    };
    match op {
        "--help" | "-h" => println!(
            "Glimpse — local universal search\n\n nixe-glimpse [--toggle] [--add-folder] [--plugins]\n nixe-glimpse sources add|remove|exclude|code PATH\n nixe-glimpse index scan|status|pause|resume|purge\n nixe-glimpse models status|setup|download [--text-only]\n nixe-glimpse plugins list|catalog [--refresh]|install ID\n nixe-glimpse plugins link-local DIR (development)\n nixe-glimpse plugins install-local DIR | install-git URL COMMIT [FOLDER]\n nixe-glimpse plugins enable ID --approve-permissions\n nixe-glimpse plugins disable|remove|update|rollback ID [--purge]\n nixe-glimpse plugins update-from DIR\n nixe-glimpse integration window|enable|disable|restore\n nixe-glimpse service enable|disable\n nixe-glimpse search QUERY\n nixe-glimpse diagnostics"
        ),
        "sources" => {
            let cmd = arg(a, 1)?;
            let root = Path::new(arg(a, 2)?);
            let _lock = crate::ai_setup::config_lock(&p)?;
            let mut c = p.load()?;
            match cmd {
                "add" => {
                    let root = root.canonicalize()?;
                    if !root.is_dir() || config::forbidden(&root) {
                        bail!("select a readable, nonhidden folder");
                    }
                    if !c.roots.contains(&root) {
                        c.roots.push(root);
                    }
                    p.save(&c)?;
                    integration::user_service(&p, true)?;
                }
                "remove" => {
                    c.roots.retain(|r| r != root);
                    c.code_roots.retain(|r| c.roots.iter().any(|p| r.starts_with(p)));
                    p.save(&c)?;
                    index::purge_outside(&index::open(&p)?, &c)?;
                    clear_semantic(&p)?;
                }
                "exclude" => {
                    c.excluded.push(root.to_owned());
                    p.save(&c)?;
                    index::purge_outside(&index::open(&p)?, &c)?;
                    clear_semantic(&p)?;
                }
                "code" => {
                    c.code_roots.push(root.to_owned());
                    p.save(&c)?;
                }
                _ => bail!("unknown source operation"),
            }
        }
        "index" => {
            let mut c = p.load()?;
            match arg(a, 1)? {
                "scan" => println!("{}", index::scan(&p, &c)?),
                "pause" => {
                    c.paused = true;
                    p.save(&c)?;
                }
                "resume" => {
                    c.paused = false;
                    p.save(&c)?;
                }
                "status" => {
                    let conn = index::open(&p)?;
                    let count: i64 = conn.query_row("SELECT count(*) FROM files", [], |r| r.get(0))?;
                    println!("{}", json!({"files":count,"paused":c.paused,"sources":c.roots.len()}));
                }
                "purge" => {
                    c.paused = true;
                    p.save(&c)?;
                    let _ = integration::user_service(&p, false);
                    index::purge(&p)?;
                }
                _ => bail!("unknown index operation"),
            }
        }
        "models" => match arg(a, 1)? {
            "status" => println!("{}", semantic::status(&p)),
            "download" => semantic::download(&p, a.iter().any(|s| s == "--text-only"))?,
            "setup" => {
                let script = runtime_script()?;
                let mut cmd = Command::new("bash");
                cmd.arg(script);
                let status = cmd.status()?;
                if !status.success() {
                    bail!("semantic runtime setup failed");
                }
            }
            _ => bail!("unknown model operation"),
        },
        "plugins" => match arg(a, 1)? {
            "list" => println!("{}", serde_json::to_string_pretty(&plugins::list(&p)?)?),
            "catalog" => {
                println!("{}", serde_json::to_string_pretty(&plugins::catalog(&p, a.iter().any(|s| s == "--refresh"))?)?)
            }
            "install" => {
                let id = arg(a, 2)?;
                if id == "quick-tools" {
                    println!("{}", plugins::install_builtin(&p)?);
                } else {
                    let e =
                        plugins::catalog(&p, false)?.into_iter().find(|e| e.id == id).context("plugin absent from catalog")?;
                    println!("{}", plugins::install_git(&p, &e.repo, &e.revision, &e.folder, false)?);
                }
            }
            "link-local" => {
                println!("{}", plugins::link_local(&p, Path::new(arg(a, 2)?))?);
            }
            "install-local" | "update-from" => {
                let dir = Path::new(arg(a, 2)?).canonicalize()?;
                println!(
                    "{}",
                    plugins::install_local(&p, &dir, &format!("local:{}", dir.display()), "", arg(a, 1)? == "update-from")?
                );
            }
            "install-git" => println!(
                "{}",
                plugins::install_git(&p, arg(a, 2)?, arg(a, 3)?, a.get(4).map(String::as_str).unwrap_or(""), false)?
            ),
            "enable" => plugins::enable(&p, arg(a, 2)?, a.iter().any(|s| s == "--approve-permissions"))?,
            "disable" => plugins::disable(&p, arg(a, 2)?)?,
            "remove" => plugins::remove(&p, arg(a, 2)?, a.iter().any(|s| s == "--purge"))?,
            "update" => {
                plugins::update(&p, arg(a, 2)?)?;
            }
            "rollback" => plugins::rollback(&p, arg(a, 2)?)?,
            _ => bail!("unknown plugin operation"),
        },
        "integration" => match arg(a, 1)? {
            "window" => {
                let enabled = fs::read_to_string(p.state.join("hypr-block.lua")).is_ok_and(|s| s.contains("o.rebind"));
                integration::apply(&p, Some(enabled))?;
            }
            "disable" => integration::apply(&p, Some(false))?,
            "enable" => integration::apply(&p, Some(true))?,
            "restore" => integration::apply(&p, None)?,
            _ => bail!("unknown integration operation"),
        },
        "service" => integration::user_service(&p, arg(a, 1)? == "enable")?,
        "search" => {
            let q = a[1..].join(" ");
            println!("{}", serde_json::to_string(&index::search(&p, &p.load()?, &q, 40)?)?);
        }
        "diagnostics" => println!(
            "{}",
            json!({"version":env!("CARGO_PKG_VERSION"),"semantic":semantic::status(&p),"sandbox_available":plugins::sandbox_probe().is_ok(),"plugins":plugins::list(&p)?.len(),"sources":p.load()?.roots.len()})
        ),
        "pages" | "describe" | "set" | "theme-changed" => settings::dispatch(&p, a)?,
        "--hide" | "--toggle" | "--query" | "--add-folder" | "--add-plugin" | "--plugins" => return Ok(false),
        _ => bail!("unknown command; use --help"),
    };
    Ok(true)
}
pub(crate) fn runtime_script() -> Result<std::path::PathBuf> {
    let exe = std::env::current_exe()?;
    let share = Paths::discover().data.join("scripts/setup-runtime.sh");
    if share.exists() {
        return Ok(share);
    }
    let dev = exe
        .parent()
        .and_then(Path::parent)
        .and_then(Path::parent)
        .context("runtime setup script missing")?
        .join("apps/glimpse/scripts/setup-runtime.sh");
    if !dev.exists() {
        bail!("runtime setup script missing");
    }
    Ok(dev)
}
pub fn clear_semantic(p: &Paths) -> Result<()> {
    // Stop only this installation's daemon and preserve its login startup preference.
    let installed = Paths::discover();
    let live = p.config == installed.config && p.data == installed.data;
    let was_running = live
        && Command::new("systemctl")
            .args(["--user", "is-active", "--quiet", "nixe-glimpsed.service"])
            .status()
            .is_ok_and(|s| s.success());
    if was_running {
        let mut c = Command::new("systemctl");
        c.args(["--user", "stop", "nixe-glimpsed.service"]);
        crate::process::capture(c, vec![], std::time::Duration::from_secs(10), 65536)?;
    }
    let dir = p.data.join("semantic-index");
    let result = if dir.exists() { fs::remove_dir_all(dir).map_err(anyhow::Error::from) } else { Ok(()) };
    if was_running {
        let mut c = Command::new("systemctl");
        c.args(["--user", "start", "nixe-glimpsed.service"]);
        crate::process::capture(c, vec![], std::time::Duration::from_secs(10), 65536)?;
    }
    result
}
