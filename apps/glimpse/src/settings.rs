use crate::{
    config::{self, Paths},
    integration,
};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    fs,
    process::{Command, Stdio},
};
const PAGE: &str = "glimpse";
fn button(key: &str, title: &str, label: &str, confirm: &str) -> Value {
    json!({"kind":"button","key":key,"title":title,"label":label,"confirm":confirm,"refresh":true})
}
fn switch(key: &str, title: &str, value: bool) -> Value {
    json!({"kind":"switch","key":key,"title":title,"value":value,"refresh":true})
}
fn info(title: &str, desc: &str) -> Value {
    json!({"kind":"info","title":title,"value":desc})
}
fn entry(key: &str, title: &str, value: &str, placeholder: &str) -> Value {
    json!({"kind":"entry","key":key,"title":title,"value":value,"placeholder":placeholder,"refresh":true})
}
fn group(title: &str, rows: Vec<Value>) -> Value {
    json!({"title":title,"rows":rows})
}
fn shortcuts_enabled(p: &Paths) -> bool {
    fs::read_to_string(p.state.join("hypr-block.lua")).is_ok_and(|s| s.contains("o.rebind"))
}
fn service_enabled() -> bool {
    Command::new("systemctl").args(["--user", "is-enabled", "nixe-glimpsed.service"]).output().is_ok_and(|o| o.status.success())
}
pub fn describe(p: &Paths, page: &str) -> Result<Value> {
    // Accept old IDs for an already-open Settings page during an upgrade, but advertise only one page.
    if page != PAGE && !page.starts_with("glimpse-") {
        bail!("unknown Settings page");
    }
    let c = p.load()?;
    let suite_path = p.config.parent().context("config parent")?.join("nixe/theme.toml");
    let suite: nixe_theme::Suite = fs::read_to_string(&suite_path).ok().and_then(|s| toml::from_str(&s).ok()).unwrap_or_default();
    let theme_options: Vec<_> = nixe_theme::all().into_iter().map(|t| (t.id, t.name)).collect();
    let service_enabled = service_enabled();
    let mut sources = vec![button("add-folder", "Search folders", "Add folder", "")];
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    for root in &c.roots {
        let full = root.display().to_string();
        let display = home
            .as_ref()
            .and_then(|home| root.strip_prefix(home).ok())
            .map(|path| format!("~/{}", path.display()))
            .unwrap_or_else(|| full.clone());
        let title = root.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| full.clone());
        sources.push(json!({"kind":"disclosure","key":format!("folder/{full}"),"title":title,"desc":display,"keywords":full,"rows":[
            switch(&format!("code-root/{full}"), "Semantic code search", c.code_roots.contains(root)),
            button(&format!("remove-root/{full}"), "Remove from search", "Remove", "Remove this folder and its indexed content?")
        ]}));
    }
    if c.roots.is_empty() {
        sources.push(info("No folders selected", "Applications and utilities work without indexed folders."));
    }
    let mut excluded = vec![entry("exclude", "Add exclusion", "", "Absolute folder path")];
    for root in &c.excluded {
        excluded.push(button(&format!("remove-exclude/{}", root.display()), &root.display().to_string(), "Remove", ""));
    }
    sources.push(json!({"kind":"disclosure","key":"exclusions","title":"Excluded folders","desc":format!("{} exclusions",c.excluded.len()),"rows":excluded}));
    let mut ai = vec![
        json!({"kind":"switch","key":"semantic","title":"Enable local AI","desc":"Search file contents and media locally. First use prepares the engine and downloads an approximately 865 MB package.","value":c.semantic_requested||c.semantic,"refresh":true}),
        json!({"kind":"info","key":"ai-status","title":"Status","value":crate::ai_setup::status(p)}),
    ];
    if crate::ai_setup::failed(p) {
        ai.push(button("ai-retry", "Setup could not finish", "Retry", ""));
    }
    let follow = matches!(suite.mode, nixe_theme::ThemeMode::Omarchy);
    let mut groups = vec![
        group(
            "General",
            vec![
                switch("preview", "Show previews", c.preview),
                switch("paused", "Pause indexing", c.paused),
                switch("service", "Start indexing at login", service_enabled),
            ],
        ),
        group(
            "Shortcuts",
            vec![
                switch("shortcuts", "Use Glimpse shortcuts", shortcuts_enabled(p)),
                json!({"kind":"entry","key":"search_key","title":"Search shortcut","value":c.search_key,"placeholder":"SUPER + SPACE","disabled":!shortcuts_enabled(p),"refresh":true}),
                json!({"kind":"entry","key":"menu_key","title":"Omarchy menu shortcut","value":c.menu_key,"placeholder":"CTRL + SPACE","disabled":!shortcuts_enabled(p),"refresh":true}),
            ],
        ),
        group("Sources", sources),
        group("Local AI", ai),
        group("Plugins", vec![button("plugins", "Manage plugins", "Open", "")]),
        group(
            "Appearance",
            vec![
                switch("follow_omarchy", "Follow Omarchy", follow),
                json!({"kind":"choice","key":"theme","title":"Theme","value":suite.theme,"options":theme_options,"disabled":follow,"refresh":true}),
            ],
        ),
    ];
    for g in &mut groups {
        if g["rows"].as_array().is_some_and(|rows| rows.len() > 3) {
            g["collapsible"] = true.into();
            g["open_by_default"] = true.into();
        }
    }
    groups[2]["note"] = format!("{} folders · {} exclusions", c.roots.len(), c.excluded.len()).into();
    Ok(json!({"poll":2,"groups":groups}))
}
fn launch(args: &[&str]) -> Result<()> {
    Command::new(std::env::current_exe()?.with_file_name("nixe-glimpse"))
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}
fn start_job(p: &Paths, op: &str) -> Result<()> {
    use std::os::fd::AsRawFd;
    p.ensure()?;
    let lock = fs::OpenOptions::new().create(true).truncate(false).write(true).open(p.state.join("settings-job.lock"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("Another operation is already running");
    }
    Command::new(std::env::current_exe()?.with_file_name("nixe-glimpse-settings"))
        .args(["run-job", op])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}
fn run_job(p: &Paths, op: &str) -> Result<()> {
    use std::os::fd::AsRawFd;
    p.ensure()?;
    let lock = fs::OpenOptions::new().create(true).truncate(false).write(true).open(p.state.join("settings-job.lock"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("Could not lock background operation");
    }
    let args: &[&str] = match op {
        "runtime" => &["models", "setup"],
        "models" => &["models", "download"],
        "rescan" => &["index", "scan"],
        "purge" => &["index", "purge"],
        _ => bail!("unknown background operation"),
    };
    let file = p.state.join("settings-job.json");
    let message = match op {
        "runtime" => "Setting up inference runtime…",
        "models" => "Downloading and verifying models…",
        "rescan" => "Refreshing indexed folders…",
        _ => "Deleting search indexes…",
    };
    config::atomic_write(&file, &serde_json::to_vec(&json!({"running":true,"pid":std::process::id(),"message":message}))?)?;
    let result = crate::cli::dispatch(&args.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    let message = match &result {
        Ok(_) => format!(
            "{} complete",
            match op {
                "runtime" => "Runtime setup",
                "models" => "Model download",
                "rescan" => "Folder refresh",
                _ => "Index deletion",
            }
        ),
        Err(e) => format!("Operation failed: {e:#}"),
    };
    config::atomic_write(&file, &serde_json::to_vec(&json!({"running":false,"message":message}))?)?;
    result.map(|_| ())
}
pub fn dispatch(p: &Paths, args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("pages") => println!(
            "{}",
            json!([{"id":PAGE,"title":"Glimpse","icon":"system-search-symbolic","description":"Search, indexing, local AI, and plugins.","keywords":"glimpse search launcher plugins"}])
        ),
        Some("describe") => println!("{}", describe(p, args.get(1).context("page missing")?)?),
        Some("theme-changed") => {}
        Some("run-ai-setup") => crate::ai_setup::run(p)?,
        Some("run-job") => run_job(p, args.get(1).context("operation missing")?)?,
        Some("set") => {
            let page = args.get(1).context("page missing")?;
            if page != PAGE && !page.starts_with("glimpse-") {
                bail!("unknown page");
            }
            let key = args.get(2).context("setting missing")?;
            let value = args.get(3).map(String::as_str).unwrap_or("");
            if key == "semantic" || key == "ai-retry" {
                crate::ai_setup::request(p, if key == "ai-retry" { true } else { value.parse()? })?;
                println!("{}", json!({"refresh":true}));
                return Ok(());
            }
            let _lock = crate::ai_setup::config_lock(p)?;
            let mut c = p.load()?;
            match key.as_str() {
                "paused" => {
                    c.paused = value.parse()?;
                    p.save(&c)?;
                }
                "battery_pause" => {
                    c.battery_pause = value.parse()?;
                    p.save(&c)?;
                }
                "preview" => {
                    c.preview = value.parse()?;
                    p.save(&c)?;
                }
                "thermal_limit" => {
                    let n: f64 = value.parse()?;
                    if !n.is_finite() || n.fract() != 0. {
                        bail!("Temperature must be a whole number");
                    }
                    c.thermal_limit = n as u32;
                    p.save(&c)?;
                }
                "device" => {
                    c.device = value.into();
                    p.save(&c)?;
                }
                "mode" | "theme" | "follow_omarchy" => {
                    let file = p.config.parent().context("config parent")?.join("nixe/theme.toml");
                    let mut suite: nixe_theme::Suite =
                        fs::read_to_string(&file).ok().and_then(|s| toml::from_str(&s).ok()).unwrap_or_default();
                    if key == "mode" || key == "follow_omarchy" {
                        let value = if key == "follow_omarchy" {
                            if value.parse::<bool>()? { "omarchy" } else { "theme" }
                        } else {
                            value
                        };
                        suite.mode = match value {
                            "omarchy" => nixe_theme::ThemeMode::Omarchy,
                            "theme" => nixe_theme::ThemeMode::Theme,
                            _ => bail!("Unknown theme source"),
                        };
                        c.mode = value.into();
                    } else {
                        if !nixe_theme::all().iter().any(|t| t.id == value) {
                            bail!("Unknown Nixe theme");
                        }
                        suite.theme = value.into();
                        c.theme = value.into();
                    }
                    p.save(&c)?;
                    config::atomic_write(&file, toml::to_string_pretty(&suite)?.as_bytes())?;
                }
                "search_key" | "menu_key" => {
                    let original = c.clone();
                    if key == "search_key" {
                        c.search_key = value.into();
                    } else {
                        c.menu_key = value.into();
                    }
                    p.save(&c)?;
                    if shortcuts_enabled(p)
                        && let Err(e) = integration::apply(p, Some(true))
                    {
                        p.save(&original)?;
                        return Err(e);
                    }
                }
                "rates_date" => {
                    if !value.trim().is_empty() && !config::valid_date(value.trim()) {
                        bail!("Use a valid YYYY-MM-DD date");
                    }
                    c.rates_date = value.trim().into();
                    p.save(&c)?;
                }
                "rates" => {
                    c.rates.clear();
                    if !value.trim().is_empty() {
                        for pair in value.split(',') {
                            let (name, rate) =
                                pair.trim().split_once('=').context("Use currency rates such as USD=1, EUR=0.92")?;
                            let name = name.trim().to_uppercase();
                            if name.len() != 3 || !name.bytes().all(|b| b.is_ascii_uppercase()) {
                                bail!("Currency codes must contain three letters");
                            }
                            if c.rates.insert(name, rate.trim().parse()?).is_some() {
                                bail!("Duplicate currency code");
                            }
                        }
                    }
                    p.save(&c)?;
                }
                "service" => integration::user_service(p, value.parse()?)?,
                "shortcuts" => integration::apply(p, Some(value.parse()?))?,
                "shortcuts-on" => integration::apply(p, Some(true))?,
                "shortcuts-off" => integration::apply(p, Some(false))?,
                "add-folder" => launch(&["--add-folder"])?,
                "plugins" => launch(&["--plugins"])?,
                "runtime" | "models" | "rescan" | "purge" => start_job(p, key)?,
                "exclude" => {
                    let root = std::path::PathBuf::from(value);
                    if !c.excluded.contains(&root) {
                        c.excluded.push(root);
                    }
                    p.save(&c)?;
                    crate::index::purge_outside(&crate::index::open(p)?, &c)?;
                    crate::cli::clear_semantic(p)?;
                }
                k if k.starts_with("remove-exclude/") => {
                    let suffix = &k[15..];
                    let i: usize = if suffix.starts_with('/') {
                        c.excluded
                            .iter()
                            .position(|p| p == std::path::Path::new(suffix))
                            .context("Exclusion changed; refresh page")?
                    } else {
                        suffix.parse()?
                    };
                    if i >= c.excluded.len() {
                        bail!("Exclusions changed; refresh page");
                    }
                    c.excluded.remove(i);
                    p.save(&c)?;
                }
                k if k.starts_with("remove-root/") => {
                    let suffix = &k[12..];
                    let i: usize = if suffix.starts_with('/') {
                        c.roots.iter().position(|p| p == std::path::Path::new(suffix)).context("Source changed; refresh page")?
                    } else {
                        suffix.parse()?
                    };
                    if i >= c.roots.len() {
                        bail!("Sources changed; refresh page");
                    }
                    c.roots.remove(i);
                    c.code_roots.retain(|r| c.roots.iter().any(|p| r.starts_with(p)));
                    p.save(&c)?;
                    crate::index::purge_outside(&crate::index::open(p)?, &c)?;
                    crate::cli::clear_semantic(p)?;
                }
                k if k.starts_with("code-root/") => {
                    let suffix = &k[10..];
                    let i: usize = if suffix.starts_with('/') {
                        c.roots.iter().position(|p| p == std::path::Path::new(suffix)).context("Source changed; refresh page")?
                    } else {
                        suffix.parse()?
                    };
                    let root = c.roots.get(i).context("Sources changed")?.clone();
                    c.code_roots.retain(|r| r != &root);
                    if value.parse::<bool>()? {
                        c.code_roots.push(root);
                    }
                    p.save(&c)?;
                }
                _ => bail!("unknown preference"),
            }
            println!(
                "{}",
                json!({"refresh":true,"toast":if matches!(key.as_str(),"runtime"|"models"|"rescan"|"purge") {"Operation started. Progress appears under Diagnostics."} else {"Glimpse updated"}})
            );
        }
        _ => bail!("unknown Settings operation"),
    }
    Ok(())
}
