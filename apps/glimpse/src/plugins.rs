use crate::{
    config::{self, Paths},
    process,
};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::os::unix::{fs::PermissionsExt, process::CommandExt};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

pub use glimpse_plugin_sdk::{API_VERSION, Action, Hit, Manifest, Permissions};
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Grant {
    pub requested: Permissions,
    pub enabled: bool,
    pub permissions: Permissions,
    pub version: String,
    pub source: String,
    pub revision: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Plugin {
    pub manifest: Manifest,
    pub grant: Grant,
    #[serde(skip)]
    pub dir: PathBuf,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub id: String,
    pub name: String,
    pub description: String,
    pub repo: String,
    pub revision: String,
    #[serde(default)]
    pub folder: String,
}
fn valid_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= 80 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}
fn registry_path(p: &Paths) -> PathBuf {
    p.config.join("plugins.json")
}
fn registry(p: &Paths) -> Result<BTreeMap<String, Grant>> {
    match fs::read(registry_path(p)) {
        Ok(s) => Ok(serde_json::from_slice(&s)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(e.into()),
    }
}
fn save_registry(p: &Paths, g: &BTreeMap<String, Grant>) -> Result<()> {
    config::atomic_write(&registry_path(p), &serde_json::to_vec_pretty(g)?)
}
/// Plugins installed before the suite was called Nyx have `nixe-<app>` ids. Their grants
/// and storage move to `nyx-<app>`; the old packages are removed, as each app's installer
/// adds the renamed one. Safe to call on every start.
pub fn migrate_nixe(p: &Paths) -> Result<()> {
    let renamed = |id: &str| id.strip_prefix("nixe-").map(|name| format!("nyx-{name}"));
    let mut reg = registry(p)?;
    let old: Vec<String> = reg.keys().filter(|id| renamed(id).is_some()).cloned().collect();
    for id in &old {
        if let (Some(new), Some(grant)) = (renamed(id), reg.remove(id)) {
            reg.entry(new).or_insert(grant);
        }
    }
    if !old.is_empty() {
        save_registry(p, &reg)?;
    }
    for (folder, keep) in [(p.data.join("plugins"), false), (p.data.join("plugin-storage"), true)] {
        let Ok(entries) = fs::read_dir(&folder) else { continue };
        for entry in entries.flatten() {
            let Some(new) = renamed(&entry.file_name().to_string_lossy()) else { continue };
            let new = folder.join(new);
            if keep && !new.exists() {
                fs::rename(entry.path(), new)?;
            } else {
                fs::remove_dir_all(entry.path())?;
            }
        }
    }
    Ok(())
}
pub fn load(dir: &Path) -> Result<Manifest> {
    let m: Manifest = toml::from_str(&fs::read_to_string(dir.join("plugin.toml"))?)?;
    if !valid_id(&m.id)
        || m.api_version != API_VERSION
        || m.exec.is_empty()
        || m.exec.len() > 32
        || m.name.len() > 200
        || m.version.is_empty()
    {
        bail!("invalid manifest or unsupported plugin API");
    }
    if m.capabilities.iter().any(|s| !["search", "commands", "preview", "reader", "settings"].contains(&s.as_str())) {
        bail!("unsupported capability");
    }
    for p in &m.permissions.read {
        if !p.is_absolute() || p == Path::new("/") || p.components().any(|x| matches!(x, std::path::Component::ParentDir)) {
            bail!("invalid plugin read scope");
        }
    }
    if m.file_types.len() > 64
        || m.file_types.iter().any(|s| s.is_empty() || s.len() > 32 || !s.bytes().all(|b| b.is_ascii_alphanumeric()))
        || m.needs.iter().any(|s| !valid_id(s))
    {
        bail!("invalid file types or dependency names");
    }
    let exe = Path::new(&m.exec[0]);
    if exe.components().count() > 1
        && (exe.is_absolute() || exe.components().any(|x| matches!(x, std::path::Component::ParentDir)))
    {
        bail!("plugin executable must be relative to package or on system PATH");
    }
    Ok(m)
}
pub fn list(p: &Paths) -> Result<Vec<Plugin>> {
    let reg = registry(p)?;
    let base = p.data.join("plugins");
    if !base.exists() {
        return Ok(vec![]);
    }
    let mut result = Vec::new();
    for entry in fs::read_dir(base)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with('.')
            || !(entry.file_type()?.is_dir() || entry.file_type()?.is_symlink() && entry.path().is_dir())
        {
            continue;
        }
        // One broken package must not take search, readers and the manager down with it.
        let manifest = match load(&entry.path()) {
            Ok(manifest) => manifest,
            Err(e) => {
                let _ = record_error(p, &entry.file_name().to_string_lossy(), &format!("invalid plugin.toml: {e:#}"));
                continue;
            }
        };
        let grant = reg.get(&manifest.id).cloned().unwrap_or_default();
        result.push(Plugin { manifest, grant, dir: entry.path() });
    }
    result.sort_by(|a, b| a.manifest.id.cmp(&b.manifest.id));
    Ok(result)
}
pub fn get(p: &Paths, id: &str) -> Result<Plugin> {
    list(p)?.into_iter().find(|x| x.manifest.id == id).context("plugin not installed")
}
fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    config::private_dir(to)?;
    for e in fs::read_dir(from)? {
        let e = e?;
        let name = e.file_name();
        if name == ".git" || name == "target" {
            continue;
        }
        let meta = e.file_type()?;
        if meta.is_symlink() {
            bail!("plugin packages cannot contain symlinks");
        }
        if meta.is_dir() {
            copy_tree(&e.path(), &to.join(name))?;
        } else if meta.is_file() {
            fs::copy(e.path(), to.join(&name))?;
            let mode = e.metadata()?.permissions().mode() & 0o700;
            fs::set_permissions(to.join(name), fs::Permissions::from_mode(mode | 0o600))?;
        } else {
            bail!("unsupported package entry");
        }
    }
    Ok(())
}
/// Copy a local package or a previously fetched, pinned Git checkout. Never run installation hooks.
pub fn install_local(p: &Paths, source: &Path, origin: &str, revision: &str, update: bool) -> Result<String> {
    p.ensure()?;
    let m = load(source)?;
    let base = p.data.join("plugins");
    config::private_dir(&base)?;
    let dest = base.join(&m.id);
    let stage = base.join(format!(".incoming-{}", m.id));
    if stage.exists() {
        bail!("another installation is pending");
    }
    if update && dest.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()) {
        bail!("development plugins update in their source folder; unlink before replacing");
    }
    if dest.exists() && !update {
        bail!("plugin already installed");
    }
    let result = (|| {
        copy_tree(source, &stage)?;
        let _ = load(&stage)?;
        let mut reg = registry(p)?;
        let old = reg.get(&m.id).cloned().unwrap_or_default();
        let enabled = old.enabled && old.requested == m.permissions;
        let rollback = base.join(format!(".rollback-{}", m.id));
        if rollback.exists() {
            fs::remove_dir_all(&rollback)?;
        }
        if dest.exists() {
            fs::rename(&dest, &rollback)?;
        }
        if let Err(e) = fs::rename(&stage, &dest) {
            if rollback.exists() {
                fs::rename(&rollback, &dest)?;
            }
            return Err(e.into());
        }
        if rollback.exists() {
            config::atomic_write(&rollback.join(".previous-grant.json"), &serde_json::to_vec(&old)?)?;
        }
        reg.insert(
            m.id.clone(),
            Grant {
                enabled,
                permissions: if enabled { old.permissions.clone() } else { Permissions::default() },
                requested: m.permissions.clone(),
                version: m.version.clone(),
                source: origin.into(),
                revision: revision.into(),
            },
        );
        if let Err(e) = save_registry(p, &reg) {
            fs::remove_dir_all(&dest)?;
            if rollback.exists() {
                fs::rename(&rollback, &dest)?;
            }
            return Err(e);
        }
        Ok(m.id.clone())
    })();
    if stage.exists() {
        let _ = fs::remove_dir_all(stage);
    }
    result
}
pub fn install_git(p: &Paths, repo: &str, revision: &str, folder: &str, update: bool) -> Result<String> {
    if !repo.starts_with("https://") || revision.len() != 40 || !revision.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("Git installs require HTTPS and a full immutable commit hash");
    }
    let rel = Path::new(folder);
    if rel.is_absolute() || rel.components().any(|x| matches!(x, std::path::Component::ParentDir)) {
        bail!("invalid package folder");
    }
    p.ensure()?;
    let temp = p.state.join(format!("fetch-{}", std::process::id()));
    if temp.exists() {
        bail!("another fetch is pending");
    }
    config::private_dir(&temp)?;
    let run = |args: &[&str]| -> Result<()> {
        let mut cmd = Command::new("git");
        cmd.args(args).current_dir(&temp).env("GIT_TERMINAL_PROMPT", "0");
        process::capture(cmd, vec![], Duration::from_secs(120), 1024 * 1024)?;
        Ok(())
    };
    let result = (|| {
        run(&["init", "-q"])?;
        run(&["fetch", "--depth", "1", repo, revision])?;
        run(&["-c", "core.hooksPath=/dev/null", "checkout", "--detach", "FETCH_HEAD"])?;
        install_local(p, &temp.join(rel), repo, revision, update)
    })();
    let _ = fs::remove_dir_all(temp);
    result
}
pub fn enable(p: &Paths, id: &str, approve: bool) -> Result<()> {
    let plugin = get(p, id)?;
    if !approve {
        bail!("enable requires --approve-permissions after reviewing the manifest");
    }
    let c = p.load()?;
    for root in &plugin.manifest.permissions.read {
        if !config::allowed(&c, root) {
            bail!("plugin scope must be a selected, nonexcluded source");
        }
    }
    for dep in &plugin.manifest.needs {
        if !on_path(dep) {
            bail!("missing dependency: {dep}");
        }
    }
    sandbox_probe()?;
    let mut reg = registry(p)?;
    let grant = reg.entry(id.into()).or_default();
    grant.enabled = true;
    grant.requested = plugin.manifest.permissions.clone();
    grant.permissions = plugin.manifest.permissions;
    if grant.permissions.selected_sources {
        grant.permissions.read.extend(c.roots.clone());
    }
    grant.version = plugin.manifest.version;
    save_registry(p, &reg)
}
pub fn disable(p: &Paths, id: &str) -> Result<()> {
    let mut reg = registry(p)?;
    if let Some(g) = reg.get_mut(id) {
        g.enabled = false;
        g.permissions = Permissions::default();
    }
    save_registry(p, &reg)
}
pub fn remove(p: &Paths, id: &str, purge: bool) -> Result<()> {
    let plugin = get(p, id)?;
    disable(p, id)?;
    if plugin.dir.symlink_metadata()?.file_type().is_symlink() {
        fs::remove_file(plugin.dir)?;
    } else {
        fs::remove_dir_all(plugin.dir)?;
    }
    let backup = p.data.join("plugins").join(format!(".rollback-{id}"));
    if backup.exists() {
        fs::remove_dir_all(backup)?;
    }
    if purge {
        for root in [&p.data, &p.config] {
            let storage = root.join("plugin-storage").join(id);
            if storage.exists() {
                fs::remove_dir_all(storage)?;
            }
        }
    }
    Ok(())
}
pub fn rollback(p: &Paths, id: &str) -> Result<()> {
    let current = get(p, id)?;
    let old = p.data.join("plugins").join(format!(".rollback-{id}"));
    let _ = load(&old)?;
    let grant: Grant = serde_json::from_slice(&fs::read(old.join(".previous-grant.json"))?)?;
    disable(p, id)?;
    let temp = current.dir.with_file_name(format!(".swap-{id}"));
    fs::rename(&current.dir, &temp)?;
    fs::rename(&old, &current.dir)?;
    fs::rename(temp, old)?;
    let mut reg = registry(p)?;
    reg.insert(id.into(), Grant { enabled: false, ..grant });
    save_registry(p, &reg)
}
pub fn on_path(s: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| std::env::split_paths(&paths).any(|p| p.join(s).is_file()))
}
pub fn sandbox_probe() -> Result<()> {
    let mut c = Command::new("bwrap");
    c.args([
        "--unshare-all",
        "--die-with-parent",
        "--ro-bind",
        "/usr",
        "/usr",
        "--symlink",
        "usr/lib",
        "/lib",
        "--symlink",
        "usr/lib",
        "/lib64",
        "--proc",
        "/proc",
        "--dev",
        "/dev",
        "--",
        "/usr/bin/true",
    ]);
    process::capture(c, vec![], Duration::from_secs(3), 1024).context("OS sandbox unavailable; plugins remain disabled")?;
    Ok(())
}
fn xdg_data_home() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
        .unwrap_or_else(|| PathBuf::from(".local/share"))
}

fn news_library_dir() -> PathBuf {
    xdg_data_home().join("nyx-news")
}

fn capture_library_dir() -> PathBuf {
    xdg_data_home().join("nyx-capture")
}

/// Read-only mount of the News library for the News plugin. Returns the database path inside the sandbox.
fn bind_news_library(cmd: &mut Command, plugin_id: &str, dir: &Path) -> Option<&'static str> {
    if plugin_id != "nyx-news" || !dir.is_dir() {
        return None;
    }
    cmd.arg("--ro-bind").arg(dir).arg("/news-library");
    Some("/news-library/news.sqlite")
}

/// Read-only mount of the Capture library for the Capture plugin. Returns the database path inside the sandbox.
fn bind_capture_library(cmd: &mut Command, plugin_id: &str, dir: &Path) -> Option<&'static str> {
    if plugin_id != "nyx-capture" || !dir.is_dir() {
        return None;
    }
    cmd.arg("--ro-bind").arg(dir).arg("/capture-library");
    Some("/capture-library/capture.sqlite")
}

fn sandbox(p: &Paths, plugin: &Plugin, read_one: Option<&Path>) -> Result<Command> {
    let cfg = p.load()?;
    if !plugin.grant.enabled
        || plugin.grant.version != plugin.manifest.version
        || plugin.grant.requested != plugin.manifest.permissions
    {
        bail!("plugin is disabled or permissions changed");
    }
    let storage = p.data.join("plugin-storage").join(&plugin.manifest.id);
    config::private_dir(&storage)?;
    let mut cmd = Command::new("bwrap");
    cmd.args([
        "--unshare-all",
        "--die-with-parent",
        "--new-session",
        "--ro-bind",
        "/usr",
        "/usr",
        "--symlink",
        "usr/lib",
        "/lib",
        "--symlink",
        "usr/lib",
        "/lib64",
        "--symlink",
        "usr/bin",
        "/bin",
        "--proc",
        "/proc",
        "--dev",
        "/dev",
        "--tmpfs",
        "/tmp",
        "--ro-bind",
    ]);
    cmd.arg(&plugin.dir).arg("/plugin").args(["--bind"]).arg(storage).arg("/storage");
    if plugin.grant.permissions.network {
        cmd.arg("--share-net");
        if Path::new("/etc/resolv.conf").exists() {
            cmd.args(["--ro-bind", "/etc/resolv.conf", "/etc/resolv.conf"]);
        }
    }
    let scopes = if let Some(file) = read_one {
        if !config::allowed(&cfg, file) || !plugin.grant.permissions.read.iter().any(|r| file.starts_with(r)) {
            bail!("reader file permission denied");
        }
        vec![file.to_owned()]
    } else {
        plugin.grant.permissions.read.clone()
    };
    for root in &scopes {
        if !config::allowed(&cfg, root) {
            bail!("source permission revoked");
        }
        cmd.arg("--ro-bind").arg(root).arg(root);
    }
    // News and Capture keep libraries under ~/.local, which selected sources cannot name.
    // Mount that one folder, not the rest of the hidden path.
    let news_db = bind_news_library(&mut cmd, &plugin.manifest.id, &news_library_dir());
    let notes_dir=xdg_data_home().join("nyx-notes/glimpse");
    let notes_db=if plugin.manifest.id=="nyx-notes"&&notes_dir.is_dir(){cmd.arg("--ro-bind").arg(&notes_dir).arg("/notes-library");Some("/notes-library/launcher.sqlite")}else{None};
    let capture_db = bind_capture_library(&mut cmd, &plugin.manifest.id, &capture_library_dir());
    // Mail, Calendar and Reminders: one database file each, nothing else of theirs.
    let pim_db = crate::pim::bind(&mut cmd, &plugin.manifest.id, &xdg_data_home());
    cmd.args([
        "--chdir",
        "/plugin",
        "--clearenv",
        "--setenv",
        "PATH",
        "/usr/bin:/bin",
        "--setenv",
        "HOME",
        "/storage",
        "--setenv",
        "GLIMPSE_STORAGE",
        "/storage",
    ]);
    if let Some(db) = news_db {
        cmd.args(["--setenv", "NYX_NEWS_DB", db]);
    }
    if let Some((key, db)) = pim_db {
        cmd.args(["--setenv", key, db]);
    }
    if let Some(db) = capture_db {
        cmd.args(["--setenv", "NYX_CAPTURE_DB", db]);
    }
    if let Some(db)=notes_db{cmd.args(["--setenv","NYX_NOTES_INDEX",db]);}
    cmd.arg("--");
    let executable = &plugin.manifest.exec[0];
    let exe = if executable.contains('/') {
        format!("/plugin/{}", executable.trim_start_matches("./"))
    } else {
        format!("/usr/bin/{executable}")
    };
    cmd.arg(exe).args(&plugin.manifest.exec[1..]);
    // Hide default secrets/generated paths and explicit exclusions even inside granted folders.
    // Folders the user can't read are skipped: the sandbox can't read them either.
    fn masks(root: &Path, cfg: &config::Config, out: &mut Vec<(PathBuf, bool)>) {
        let Ok(entries) = fs::read_dir(root) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else { continue };
            if config::forbidden(&path) || cfg.excluded.iter().any(|p| path.starts_with(p)) {
                out.push((path, kind.is_dir()));
            } else if kind.is_dir() {
                masks(&path, cfg, out);
            }
        }
    }
    let mut denied = Vec::new();
    for root in &scopes {
        if root.is_dir() {
            masks(root, &cfg, &mut denied);
        }
    }
    // Mount exclusions must precede the executable separator.
    let mut all: Vec<std::ffi::OsString> = cmd.get_args().map(|s| s.to_owned()).collect();
    let separator = all.iter().position(|s| s == "--").context("sandbox argument separator")?;
    let mut mask_args = Vec::new();
    for (path, dir) in denied {
        if dir {
            mask_args.push("--tmpfs".into());
            mask_args.push(path.into_os_string());
        } else {
            mask_args.push("--ro-bind".into());
            mask_args.push("/dev/null".into());
            mask_args.push(path.into_os_string());
        }
    }
    all.splice(separator..separator, mask_args);
    let mut rebuilt = Command::new("bwrap");
    rebuilt.args(all);
    cmd = rebuilt;
    // SAFETY: pre_exec calls only async-signal-safe libc resource-limit operations.
    unsafe {
        cmd.pre_exec(|| {
            let cpu = libc::rlimit { rlim_cur: 10, rlim_max: 10 };
            let mem = libc::rlimit { rlim_cur: 512 * 1024 * 1024, rlim_max: 512 * 1024 * 1024 };
            if libc::setrlimit(libc::RLIMIT_CPU, &cpu) != 0 || libc::setrlimit(libc::RLIMIT_AS, &mem) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(cmd)
}
pub fn call(p: &Paths, id: &str, capability: &str, op: &str, args: Value) -> Result<Value> {
    call_cancel(p, id, capability, op, args, || false)
}
pub fn call_cancel(p: &Paths, id: &str, capability: &str, op: &str, args: Value, cancelled: impl Fn() -> bool) -> Result<Value> {
    let plugin = get(p, id)?;
    if !plugin.manifest.capabilities.iter().any(|s| s == capability) {
        bail!("capability not advertised");
    }
    let read_one = if op == "extract" { Some(Path::new(args["path"].as_str().context("reader path missing")?)) } else { None };
    let input = format!(
        "{}\n{}\n{}\n",
        json!({"api_version":1,"id":0,"op":"hello"}),
        json!({"api_version":1,"id":1,"op":op,"args":args}),
        json!({"api_version":1,"id":2,"op":"shutdown"})
    );

    let revoked = process::throttled(Duration::from_millis(250), || {
        get(p, id).is_ok_and(|now| {
            !now.grant.enabled || now.grant.version != plugin.grant.version || now.grant.permissions != plugin.grant.permissions
        })
    });
    let out = process::capture_cancel(
        sandbox(p, &plugin, read_one)?,
        input.into_bytes(),
        Duration::from_secs(if op == "extract" { 10 } else { 2 }),
        1024 * 1024,
        || cancelled() || revoked(),
    )?;
    let messages: Vec<Value> =
        out.split(|b| *b == b'\n').filter(|l| !l.is_empty()).map(serde_json::from_slice).collect::<serde_json::Result<_>>()?;
    if !messages.iter().any(|v| v["id"] == 0 && v["api_version"] == 1) {
        bail!("plugin handshake failed");
    }
    // Revocation/update during execution invalidates the reply.
    let current = get(p, id)?;
    if !current.grant.enabled
        || current.grant.version != plugin.grant.version
        || current.grant.permissions != plugin.grant.permissions
    {
        bail!("plugin changed while running");
    }
    let reply = messages.into_iter().find(|v| v["id"] == 1).context("missing plugin response")?;
    if let Some(e) = reply.get("error") {
        bail!("plugin: {e}");
    }
    let _ = fs::remove_file(p.state.join("plugin-errors").join(id));
    Ok(reply["result"].clone())
}
pub fn search(p: &Paths, q: &str) -> Vec<(String, Hit)> {
    search_cancel(p, q, || false)
}
pub fn search_cancel(p: &Paths, q: &str, cancelled: impl Fn() -> bool) -> Vec<(String, Hit)> {
    let mut result = Vec::new();
    if let Ok(plugins) = list(p) {
        for plugin in plugins {
            if cancelled() {
                break;
            }
            if plugin.grant.enabled
                && (plugin.manifest.capabilities.contains(&"search".into())
                    || plugin.manifest.capabilities.contains(&"commands".into()))
            {
                let cap = if plugin.manifest.capabilities.contains(&"search".into()) { "search" } else { "commands" };
                match call_cancel(p, &plugin.manifest.id, cap, "search", json!({"query":q,"limit":10}), &cancelled)
                    .and_then(|v| Ok(serde_json::from_value::<Vec<Hit>>(v)?))
                {
                    Ok(hits) => {
                        for hit in hits.into_iter().take(10) {
                            if hit.title.len() <= 512 && hit.subtitle.len() <= 2048 && valid_action(p, &plugin, &hit.action) {
                                result.push((plugin.manifest.id.clone(), hit));
                            }
                        }
                    }
                    Err(e) => {
                        let _ = record_error(p, &plugin.manifest.id, &e.to_string());
                    }
                }
            }
        }
    }
    result
}
pub fn valid_action(p: &Paths, plugin: &Plugin, a: &Action) -> bool {
    match a {
        Action::Copy { text } => text.len() <= 65536,
        Action::Open { path } => {
            p.load().is_ok_and(|c| config::allowed(&c, path)) && plugin.grant.permissions.read.iter().any(|r| path.starts_with(r))
        }
        Action::Url { url } => {
            plugin.grant.permissions.network && (url.starts_with("https://") || url.starts_with("http://")) && url.len() < 4096
        }
        Action::Activate { token } => token.len() <= 4096,
    }
}
/// Host-side Capture commands. The plugin cannot spawn GTK; Glimpse launches `nyx-capture` instead.
pub fn capture_host_args(plugin_id: &str, token: &str) -> Option<&'static str> {
    if plugin_id != "nyx-capture" {
        return None;
    }
    match token {
        "region" => Some("--region"),
        "window" => Some("--window"),
        "fullscreen" => Some("--fullscreen"),
        "scroll" => Some("--scroll"),
        _ => None,
    }
}

pub fn spawn_capture(flag: &str) -> Result<()> {
    let exe = capture_executable();
    Command::new(&exe).arg(flag).spawn().with_context(|| format!("couldn't start {}", exe.display()))?;
    Ok(())
}

fn capture_executable() -> PathBuf {
    if let Some(home) = std::env::var_os("HOME") {
        let local = PathBuf::from(home).join(".local/bin/nyx-capture");
        if local.is_file() {
            return local;
        }
    }
    PathBuf::from("nyx-capture")
}

pub fn activate(p: &Paths, id: &str, token: &str) -> Result<Action> {
    let plugin = get(p, id)?;
    let cap = if plugin.manifest.capabilities.contains(&"commands".into()) { "commands" } else { "search" };
    let a: Action = serde_json::from_value(call(p, id, cap, "activate", json!({"token":token}))?)?;
    if matches!(a, Action::Activate { .. }) || !valid_action(p, &plugin, &a) {
        bail!("plugin returned an invalid host action");
    }
    Ok(a)
}
pub fn record_error(p: &Paths, id: &str, e: &str) -> Result<()> {
    if !valid_id(id) {
        bail!("invalid plugin ID");
    }
    let message: String = e.chars().filter(|c| !c.is_control()).take(300).collect();
    config::atomic_write(&p.state.join("plugin-errors").join(id), message.as_bytes())
}
/// The last recorded failure for a plugin, for display.
pub fn last_error(p: &Paths, id: &str) -> Option<String> {
    if !valid_id(id) {
        return None;
    }
    fs::read_to_string(p.state.join("plugin-errors").join(id)).ok().filter(|s| !s.trim().is_empty())
}
pub fn catalog(p: &Paths, refresh: bool) -> Result<Vec<CatalogEntry>> {
    let cache = p.data.join("plugin-catalog.json");
    if refresh {
        let fetched = (|| -> Result<()> {
            let mut c = Command::new("curl");
            c.args([
                "-fLsS",
                "--max-time",
                "10",
                "https://raw.githubusercontent.com/design-nexus/nyx-glimpse/main/apps/glimpse/data/plugins.json",
            ]);
            let out = process::capture(c, vec![], Duration::from_secs(12), 1024 * 1024)?;
            let _: Vec<CatalogEntry> = serde_json::from_slice(&out).context("invalid online catalog")?;
            config::atomic_write(&cache, &out)
        })();
        let message = match fetched {
            Ok(()) => "Catalog refreshed.",
            Err(_) if cache.exists() => "Online catalog unavailable. Using the last saved catalog.",
            Err(_) => "Online catalog unavailable. Using the bundled catalog.",
        };
        config::atomic_write(&p.state.join("catalog-status.txt"), message.as_bytes())?;
    }
    if let Ok(bytes) = fs::read(cache)
        && let Ok(catalog) = serde_json::from_slice(&bytes)
    {
        return Ok(catalog);
    }
    Ok(serde_json::from_slice(include_bytes!("../data/plugins.json"))?)
}
pub fn update(p: &Paths, id: &str) -> Result<String> {
    let plugin = get(p, id)?;
    if plugin.grant.source == "builtin:quick-tools" {
        return install_builtin_mode(p, true);
    }
    if let Some(folder) = plugin.grant.source.strip_prefix("local:") {
        return install_local(p, Path::new(folder), &plugin.grant.source, "", true);
    }
    if plugin.grant.source.starts_with("dev:") {
        bail!("development plugins use their source directly; enable again after a manifest change");
    }
    let entry =
        catalog(p, true)?.into_iter().find(|e| e.id == id).context("no catalog entry; use update-from for custom sources")?;
    let result = install_git(p, &entry.repo, &entry.revision, &entry.folder, true)?;
    if result != plugin.manifest.id {
        bail!("catalog identity mismatch");
    }
    Ok(result)
}

pub fn install_builtin(p: &Paths) -> Result<String> {
    install_builtin_mode(p, false)
}
fn install_builtin_mode(p: &Paths, update: bool) -> Result<String> {
    let source = p.state.join("bundled-quick-tools");
    config::private_dir(&source)?;
    config::atomic_write(&source.join("plugin.toml"), include_bytes!("../examples/hello/plugin.toml"))?;
    config::atomic_write(&source.join("main.py"), include_bytes!("../examples/hello/main.py"))?;
    install_local(p, &source, "builtin:quick-tools", "bundled-v1", update)
}

/// Explicit development mode: load a local source folder afresh on every request.
pub fn link_local(p: &Paths, source: &Path) -> Result<String> {
    let source = source.canonicalize()?;
    let manifest = load(&source)?;
    let base = p.data.join("plugins");
    config::private_dir(&base)?;
    let dest = base.join(&manifest.id);
    if dest.symlink_metadata().is_ok() {
        bail!("plugin already installed");
    }
    std::os::unix::fs::symlink(&source, &dest)?;
    let mut reg = registry(p)?;
    reg.insert(
        manifest.id.clone(),
        Grant {
            source: format!("dev:{}", source.display()),
            version: manifest.version,
            requested: manifest.permissions,
            ..Grant::default()
        },
    );
    if let Err(e) = save_registry(p, &reg) {
        fs::remove_file(dest)?;
        return Err(e);
    }
    Ok(manifest.id)
}

// Notes host-action profile v1. Keep the plugin sandbox read-only: the host launches
// only the installed Notes executable with one validated JSON argument, never a shell.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NotesHostRequest {api_version:u32,op:String,vault:PathBuf,#[serde(default)]path:Option<PathBuf>,#[serde(default)]title:Option<String>,#[serde(default)]offset:Option<u64>,#[serde(default)]append:bool}
fn notes_host_args(token:&str)->Result<Vec<String>>{
    if token.len()>4096{bail!("Notes action exceeds limit");}let request:NotesHostRequest=serde_json::from_str(token)?;
    if request.api_version!=1||!request.vault.is_absolute()||request.vault.components().any(|c|c==std::path::Component::ParentDir){bail!("Invalid Notes vault");}
    match request.op.as_str(){"create"=>{let title=request.title.context("Missing note title")?;if title.is_empty()||title.len()>240||title.starts_with('.')||title.contains(['/','\\','\0','\n','\r']){bail!("Invalid note title");}},"open"=>{let path=request.path.context("Missing note path")?;if !path.starts_with(&request.vault)||path.components().any(|c|c==std::path::Component::ParentDir||c.as_os_str()==".obsidian"){bail!("Note must remain inside its vault");}},_=>bail!("Unsupported Notes operation")};
    let _=(request.offset,request.append);Ok(vec!["--launcher-action".into(),token.into()])
}
pub fn notes_host_action(paths:&Paths,id:&str,token:&str)->Option<Result<()>>{
    if id!="nyx-notes"{return None;}Some((||{let plugin=get(paths,id)?;if !plugin.grant.enabled||plugin.grant.version!=plugin.manifest.version||plugin.grant.requested!=plugin.manifest.permissions{bail!("Notes plugin permission was revoked or changed");}let args=notes_host_args(token)?;let local=std::env::var_os("HOME").map(PathBuf::from).map(|h|h.join(".local/bin/nyx-notes"));let exe=local.filter(|p|p.is_file()).unwrap_or_else(||PathBuf::from("nyx-notes"));Command::new(&exe).args(args).spawn().with_context(||format!("Could not start {}",exe.display()))?;Ok(())})())
}
#[cfg(test)]mod notes_action_tests{use super::*;#[test]fn notes_actions_never_execute_arbitrary_commands(){assert!(notes_host_args(r#"{"api_version":1,"op":"create","vault":"/home/test/Notes","title":"Research; echo nope"}"#).is_ok());assert!(notes_host_args(r#"{"api_version":1,"op":"exec","vault":"/home/test/Notes","title":"shell"}"#).is_err());assert!(notes_host_args(r#"{"api_version":1,"op":"open","vault":"/home/test/Notes","path":"/home/test/Notes/../secret.md"}"#).is_err());assert!(notes_host_args(r#"{"api_version":1,"op":"create","vault":"/home/test/Notes","title":"../escape"}"#).is_err());}}
#[cfg(test)]
mod rename_tests {
    use super::*;

    #[test]
    fn nixe_plugins_move_to_their_nyx_ids() {
        let root = tempfile::tempdir().unwrap();
        let p = Paths { config: root.path().join("c"), data: root.path().join("d"), state: root.path().join("s") };
        fs::create_dir_all(&p.config).unwrap();
        fs::create_dir_all(p.data.join("plugins/nixe-notes")).unwrap();
        fs::create_dir_all(p.data.join("plugin-storage/nixe-mail")).unwrap();
        fs::write(p.data.join("plugin-storage/nixe-mail/state"), "kept").unwrap();
        let grant = Grant { enabled: true, version: "0.5.0".into(), ..Default::default() };
        save_registry(&p, &BTreeMap::from([("nixe-notes".to_string(), grant)])).unwrap();

        migrate_nixe(&p).unwrap();
        let reg = registry(&p).unwrap();
        assert!(reg["nyx-notes"].enabled && !reg.contains_key("nixe-notes"));
        assert!(!p.data.join("plugins/nixe-notes").exists());
        assert_eq!(fs::read_to_string(p.data.join("plugin-storage/nyx-mail/state")).unwrap(), "kept");
        // A second run changes nothing.
        migrate_nixe(&p).unwrap();
        assert!(registry(&p).unwrap()["nyx-notes"].enabled);
    }
}
