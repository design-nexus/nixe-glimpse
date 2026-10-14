use crate::{
    config::{self, Paths},
    process, semantic,
};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    cell::Cell,
    fs::{self, File, OpenOptions},
    os::fd::AsRawFd,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
pub(crate) fn config_lock(p: &Paths) -> Result<File> {
    p.ensure()?;
    let file = OpenOptions::new().create(true).truncate(false).write(true).open(p.state.join("preferences.lock"))?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
        bail!("Could not lock preferences");
    }
    Ok(file)
}
fn state(p: &Paths) -> Value {
    fs::read(p.state.join("ai-setup.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null)
}
fn alive(s: &Value) -> bool {
    s["running"] == true && s["pid"].as_u64().is_some_and(|pid| unsafe { libc::kill(pid as i32, 0) } == 0)
}
fn record(p: &Paths, running: bool, failed: bool, stage: &str, message: &str) -> Result<()> {
    config::atomic_write(
        &p.state.join("ai-setup.json"),
        &serde_json::to_vec(
            &json!({"running":running,"failed":failed,"pid":std::process::id(),"stage":stage,"message":message}),
        )?,
    )
}
fn assets_present(p: &Paths) -> bool {
    semantic::runtime(p).is_file()
        && semantic::MODELS
            .iter()
            .all(|(name, size, _)| p.data.join("Models").join(name).metadata().is_ok_and(|m| m.len() == *size))
}
pub fn status(p: &Paths) -> String {
    let c = p.load().unwrap_or_default();
    if !c.semantic_requested && !c.semantic {
        return "Disabled".into();
    }
    let s = state(p);
    if alive(&s) {
        if s["stage"] == "download" {
            let bytes: u64 = semantic::MODELS
                .iter()
                .map(|(name, size, _)| {
                    p.data.join("Models").join(name).metadata().map(|m| m.len().min(*size)).unwrap_or_else(|_| {
                        p.data.join("Models").join(format!("{name}.partial")).metadata().map(|m| m.len().min(*size)).unwrap_or(0)
                    })
                })
                .sum();
            return format!(
                "Downloading and verifying · {}%",
                bytes * 100 / semantic::MODELS.iter().map(|(_, s, _)| s).sum::<u64>()
            );
        }
        return s["message"].as_str().unwrap_or("Preparing local AI…").into();
    }
    if c.semantic {
        return if c.roots.is_empty() { "Ready · add folders to search their contents" } else { "Ready · processing locally" }
            .into();
    }
    if s["failed"] == true {
        return s["message"].as_str().unwrap_or("Setup failed. Retry to continue.").into();
    }
    if s.is_null() { "Preparing local AI…".into() } else { "Setup interrupted · open Glimpse to resume, or retry".into() }
}
pub fn failed(p: &Paths) -> bool {
    let s = state(p);
    s["failed"] == true && !alive(&s) && p.load().is_ok_and(|c| c.semantic_requested && !c.semantic)
}
pub fn request(p: &Paths, enabled: bool) -> Result<()> {
    {
        let _lock = config_lock(p)?;
        let mut c = p.load()?;
        c.semantic_requested = enabled;
        c.semantic = enabled && c.semantic && assets_present(p);
        if enabled {
            c.device = "auto".into();
        }
        p.save(&c)?;
    }
    if enabled {
        spawn(p, true)?;
    }
    Ok(())
}
pub fn resume(p: &Paths) -> Result<()> {
    spawn(p, false)
}
fn spawn(p: &Paths, retry: bool) -> Result<()> {
    let c = p.load()?;
    if !c.semantic_requested || c.semantic {
        return Ok(());
    }
    let s = state(p);
    if alive(&s) || (!retry && s["failed"] == true) {
        return Ok(());
    }
    Command::new(std::env::current_exe()?.with_file_name("nixe-glimpse-settings"))
        .args(["run-ai-setup"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}
pub fn run(p: &Paths) -> Result<()> {
    run_steps(
        p,
        |cancelled| {
            let mut command = Command::new("bash");
            command.arg(crate::cli::runtime_script()?);
            process::capture_cancel(command, vec![], Duration::from_secs(1800), 128 * 1024, cancelled)
                .context("Could not prepare local AI runtime")
                .map(|_| ())
        },
        |cancelled| semantic::download_cancel(p, false, cancelled).context("Could not download or verify the AI package"),
        || crate::integration::start_indexing(p),
    )
}
fn run_steps(
    p: &Paths,
    prepare: impl FnOnce(&dyn Fn() -> bool) -> Result<()>,
    download: impl FnOnce(&dyn Fn() -> bool) -> Result<()>,
    start_indexing: impl FnOnce() -> Result<()>,
) -> Result<()> {
    p.ensure()?;
    let job = OpenOptions::new().create(true).truncate(false).write(true).open(p.state.join("ai-setup.lock"))?;
    if unsafe { libc::flock(job.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Ok(());
    }
    if !p.load()?.semantic_requested {
        return Ok(());
    }
    let last = Cell::new(Instant::now() - Duration::from_secs(1));
    let cached = Cell::new(false);
    let saw_cancel = Cell::new(false);
    let cancelled = || {
        if last.get().elapsed() > Duration::from_millis(200) {
            let cancel = !p.load().is_ok_and(|c| c.semantic_requested);
            cached.set(cancel);
            last.set(Instant::now());
            if cancel {
                saw_cancel.set(true);
            }
        }
        cached.get()
    };
    let result = (|| -> Result<()> {
        if !semantic::runtime(p).is_file() {
            record(p, true, false, "runtime", "Preparing local AI…")?;
            prepare(&cancelled)?;
        }
        if cancelled() {
            bail!("operation cancelled");
        }
        record(p, true, false, "download", "Downloading and verifying…")?;
        download(&cancelled)?;
        let _lock = config_lock(p)?;
        let mut c = p.load()?;
        if !c.semantic_requested {
            bail!("operation cancelled");
        }
        if !c.roots.is_empty() {
            start_indexing()?;
        }
        c.semantic = true;
        c.device = "auto".into();
        p.save(&c)?;
        Ok(())
    })();
    let wanted = p.load()?.semantic_requested;
    let retry = wanted && saw_cancel.get() && result.is_err();
    let message = match &result {
        Ok(()) => "Ready · processing locally".into(),
        Err(_) if !wanted => "Disabled".into(),
        Err(e) => format!("Setup failed: {e:#}"),
    };
    record(p, false, result.is_err() && wanted && !retry, "finished", &message)?;
    drop(job);
    if retry {
        spawn(p, true)?;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let p = Paths { config: dir.path().join("config"), data: dir.path().join("data"), state: dir.path().join("state") };
        p.ensure().unwrap();
        p.save(&crate::config::Config { semantic_requested: true, ..Default::default() }).unwrap();
        (dir, p)
    }
    #[test]
    fn simulated_setup_obeys_requested_state() {
        let (_dir, p) = fixture();
        let prepared = Cell::new(false);
        let downloaded = Cell::new(false);
        run_steps(
            &p,
            |_| {
                prepared.set(true);
                Ok(())
            },
            |_| {
                downloaded.set(true);
                Ok(())
            },
            || Ok(()),
        )
        .unwrap();
        assert!(prepared.get() && downloaded.get() && p.load().unwrap().semantic);
        let (_dir, p) = fixture();
        assert!(
            run_steps(
                &p,
                |_| Ok(()),
                |_| {
                    let mut c = p.load()?;
                    c.semantic_requested = false;
                    p.save(&c)?;
                    Ok(())
                },
                || Ok(())
            )
            .is_err()
        );
        assert!(!p.load().unwrap().semantic);
        assert_eq!(status(&p), "Disabled");
        let (_dir, p) = fixture();
        assert!(
            run_steps(&p, |_| bail!("simulated preparation failure"), |_| panic!("download must not start"), || Ok(())).is_err()
        );
        assert!(failed(&p));
        assert!(!p.load().unwrap().semantic);
    }
}
