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
        let manifest = load(&entry.path())?;
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
        "--",
    ]);
    let executable = &plugin.manifest.exec[0];
    let exe = if executable.contains('/') {
        format!("/plugin/{}", executable.trim_start_matches("./"))
    } else {
        format!("/usr/bin/{executable}")
    };
    cmd.arg(exe).args(&plugin.manifest.exec[1..]);
    // Hide default secrets/generated paths and explicit exclusions even inside granted folders.
    fn masks(root: &Path, cfg: &config::Config, out: &mut Vec<(PathBuf, bool)>) -> Result<()> {
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            let path = entry.path();
            let kind = entry.file_type()?;
            if config::forbidden(&path) || cfg.excluded.iter().any(|p| path.starts_with(p)) {
                out.push((path, kind.is_dir()));
            } else if kind.is_dir() {
                masks(&path, cfg, out)?;
            }
        }
        Ok(())
    }
    let mut denied = Vec::new();
    for root in &scopes {
        if root.is_dir() {
            masks(root, &cfg, &mut denied)?;
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

    let out = process::capture_cancel(
        sandbox(p, &plugin, read_one)?,
        input.into_bytes(),
        Duration::from_secs(if op == "extract" { 10 } else { 2 }),
        1024 * 1024,
        || {
            cancelled()
                || get(p, id).is_ok_and(|now| {
                    !now.grant.enabled
                        || now.grant.version != plugin.grant.version
                        || now.grant.permissions != plugin.grant.permissions
                })
        },
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
pub fn activate(p: &Paths, id: &str, token: &str) -> Result<Action> {
    let plugin = get(p, id)?;
    let cap = if plugin.manifest.capabilities.contains(&"commands".into()) { "commands" } else { "search" };
    let a: Action = serde_json::from_value(call(p, id, cap, "activate", json!({"token":token}))?)?;
    if matches!(a, Action::Activate { .. }) || !valid_action(p, &plugin, &a) {
        bail!("plugin returned an invalid host action");
    }
    Ok(a)
}
pub fn record_error(p: &Paths, id: &str, _e: &str) -> Result<()> {
    if !valid_id(id) {
        bail!("invalid plugin ID");
    }
    config::atomic_write(
        &p.state.join("plugin-errors").join(id),
        b"Plugin request failed: timeout, invalid response, revoked permissions, or unavailable dependency.",
    )
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
                "https://raw.githubusercontent.com/design-nexus/nixe/main/apps/glimpse/data/plugins.json",
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
