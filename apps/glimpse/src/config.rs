use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub version: u32,
    pub roots: Vec<PathBuf>,
    pub excluded: Vec<PathBuf>,
    pub code_roots: Vec<PathBuf>,
    pub paused: bool,
    pub battery_pause: bool,
    pub thermal_limit: u32,
    pub semantic: bool,
    pub semantic_requested: bool,
    pub device: String,
    pub search_key: String,
    pub menu_key: String,
    pub preview: bool,
    pub mode: String,
    pub theme: String,
    pub rates_date: String,
    pub rates: BTreeMap<String, f64>,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            version: 1,
            roots: vec![],
            excluded: vec![],
            code_roots: vec![],
            paused: false,
            battery_pause: true,
            thermal_limit: 85,
            semantic: false,
            semantic_requested: false,
            device: "auto".into(),
            search_key: "SUPER + SPACE".into(),
            menu_key: "CTRL + SPACE".into(),
            preview: true,
            mode: "omarchy".into(),
            theme: "tokyo-night".into(),
            rates_date: String::new(),
            rates: BTreeMap::new(),
        }
    }
}
#[derive(Clone, Debug)]
pub struct Paths {
    pub config: PathBuf,
    pub data: PathBuf,
    pub state: PathBuf,
}
impl Paths {
    pub fn discover() -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/nonexistent"));
        let xdg = |key: &str, fallback: &str| {
            std::env::var_os(key)
                .map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .unwrap_or_else(|| home.join(fallback))
                .join("nixe-glimpse")
        };
        Self {
            config: xdg("XDG_CONFIG_HOME", ".config"),
            data: xdg("XDG_DATA_HOME", ".local/share"),
            state: xdg("XDG_STATE_HOME", ".local/state"),
        }
    }
    pub fn config_file(&self) -> PathBuf {
        self.config.join("settings.toml")
    }
    pub fn load(&self) -> Result<Config> {
        let c = match fs::read_to_string(self.config_file()) {
            Ok(s) => toml::from_str(&s).context("invalid Glimpse preferences")?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Config::default(),
            Err(e) => return Err(e.into()),
        };
        validate(&c)?;
        Ok(c)
    }
    pub fn save(&self, c: &Config) -> Result<()> {
        validate(c)?;
        atomic_write(&self.config_file(), toml::to_string_pretty(c)?.as_bytes())
    }
    pub fn ensure(&self) -> Result<()> {
        for p in [&self.config, &self.data, &self.state] {
            private_dir(p)?;
        }
        Ok(())
    }
}
pub fn private_dir(p: &Path) -> Result<()> {
    if p.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()) {
        bail!("private directory is a symlink: {}", p.display());
    }
    fs::create_dir_all(p)?;
    fs::set_permissions(p, fs::Permissions::from_mode(0o700))?;
    Ok(())
}
pub fn atomic_write(p: &Path, bytes: &[u8]) -> Result<()> {
    let parent = p.parent().context("missing parent")?;
    private_dir(parent)?;
    let temp = parent.join(format!(
        ".write-{}-{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos()
    ));
    let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temp)?;
    let result = (|| {
        f.write_all(bytes)?;
        f.sync_all()?;
        fs::rename(&temp, p)?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result
}
pub fn validate(c: &Config) -> Result<()> {
    for key in [&c.search_key, &c.menu_key] {
        validate_key(key)?;
    }
    if c.search_key.split('+').map(str::trim).collect::<Vec<_>>() == c.menu_key.split('+').map(str::trim).collect::<Vec<_>>() {
        bail!("Search and menu shortcuts must differ");
    }
    if c.version != 1 {
        bail!("unsupported preferences version");
    }
    if !(50..=110).contains(&c.thermal_limit) {
        bail!("thermal limit must be 50–110 °C");
    }
    if !["auto", "cpu", "cuda", "vulkan"].contains(&c.device.as_str()) {
        bail!("invalid inference device");
    }
    if !["omarchy", "theme"].contains(&c.mode.as_str()) {
        bail!("invalid theme mode");
    }
    for p in c.roots.iter().chain(&c.excluded).chain(&c.code_roots) {
        if !p.is_absolute() || p.components().any(|x| matches!(x, std::path::Component::ParentDir)) || p == Path::new("/") {
            bail!("source paths must be absolute, normalized folders other than /");
        }
    }
    if c.code_roots.iter().any(|p| !c.roots.iter().any(|r| p.starts_with(r))) {
        bail!("code folders must be within selected sources");
    }
    if c.rates.values().any(|r| !r.is_finite() || *r <= 0.0) {
        bail!("rates must be positive finite values");
    }
    if !c.rates.is_empty() && !valid_date(&c.rates_date) {
        bail!("currency rates need a YYYY-MM-DD date");
    }
    Ok(())
}
pub fn valid_date(s: &str) -> bool {
    let a: Vec<_> = s.split('-').collect();
    if a.len() != 3 || a[0].len() != 4 || a[1].len() != 2 || a[2].len() != 2 {
        return false;
    }
    let (Ok(y), Ok(m), Ok(d)) = (a[0].parse::<u32>(), a[1].parse::<u32>(), a[2].parse::<u32>()) else {
        return false;
    };
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let max = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        _ => 0,
    };
    y >= 1900 && d > 0 && d <= max
}
pub fn forbidden(p: &Path) -> bool {
    p.components().any(|c| {
        let s = c.as_os_str().to_string_lossy().to_lowercase();
        s.starts_with('.')
            || [
                "node_modules",
                "target",
                "vendor",
                "dist",
                "build",
                "cache",
                "caches",
                "credentials",
                "secrets",
                "id_rsa",
                "id_ed25519",
                "passwords.txt",
                "credentials.json",
                "credentials.toml",
                "token.json",
            ]
            .contains(&s.as_str())
            || [".pem", ".key", ".p12", ".pfx", ".crt", ".cer"].iter().any(|ext| s.ends_with(ext))
    })
}
pub fn allowed(c: &Config, p: &Path) -> bool {
    if forbidden(p) || !c.roots.iter().any(|r| p.starts_with(r)) || c.excluded.iter().any(|r| p.starts_with(r)) {
        return false;
    }
    // Reject symlinks in every existing component, including replaced ancestors.
    let mut current = PathBuf::new();
    for part in p.components() {
        current.push(part);
        if current.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()) {
            return false;
        }
    }
    true
}

pub fn validate_key(key: &str) -> Result<()> {
    let parts: Vec<_> = key.split('+').map(str::trim).collect();
    if parts.len() < 2 || parts.len() > 5 {
        bail!("shortcut needs modifiers and one key");
    }
    let mut seen = std::collections::HashSet::new();
    for modifier in &parts[..parts.len() - 1] {
        if !["SUPER", "CTRL", "ALT", "SHIFT"].contains(modifier) || !seen.insert(*modifier) {
            bail!("invalid or duplicate shortcut modifier");
        }
    }
    let key = parts.last().unwrap();
    if !(key.len() == 1 && key.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        || ["SPACE", "RETURN", "TAB", "ESCAPE"].contains(key)
        || key.strip_prefix('F').and_then(|s| s.parse::<u32>().ok()).is_some_and(|n| (1..=12).contains(&n)))
    {
        bail!("unsupported shortcut key");
    }
    Ok(())
}
