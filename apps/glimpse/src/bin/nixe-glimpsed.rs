use nixe_glimpse::{
    config::{Paths, atomic_write},
    index, semantic,
};
use notify::{RecursiveMode, Watcher};
use std::{
    fs,
    os::fd::AsRawFd,
    sync::mpsc,
    time::{Duration, Instant},
};
fn gated(c: &nixe_glimpse::config::Config) -> bool {
    if c.battery_pause
        && fs::read_dir("/sys/class/power_supply").is_ok_and(|entries| {
            entries.flatten().any(|e| fs::read_to_string(e.path().join("status")).is_ok_and(|s| s.trim() == "Discharging"))
        })
    {
        return true;
    }
    fs::read_dir("/sys/class/thermal").is_ok_and(|entries| {
        entries.flatten().any(|e| {
            fs::read_to_string(e.path().join("temp"))
                .ok()
                .and_then(|s| s.trim().parse::<u32>().ok())
                .is_some_and(|t| t / 1000 >= c.thermal_limit)
        })
    })
}
fn run() -> anyhow::Result<()> {
    let p = Paths::discover();
    p.ensure()?;
    let lock = fs::OpenOptions::new().create(true).truncate(false).write(true).open(p.state.join("daemon.lock"))?;
    // SAFETY: valid owned file descriptor, held for the process lifetime.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        anyhow::bail!("Glimpse indexer already running");
    }
    let (tx, rx) = mpsc::channel();
    let config_file = p.config_file();
    let config_folder = p.config.clone();
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if let Ok(event) = event
            && matches!(event.kind, notify::EventKind::Create(_) | notify::EventKind::Modify(_) | notify::EventKind::Remove(_))
            && event.paths.iter().any(|path| path != &config_folder || path == &config_file)
        {
            let _ = tx.send(());
        }
    })?;
    watcher.watch(&p.config, RecursiveMode::NonRecursive)?;
    let mut watched: Vec<std::path::PathBuf> = Vec::new();
    let mut last = Instant::now() - Duration::from_secs(60);
    loop {
        let c = p.load()?;
        if c.roots != watched {
            for root in &watched {
                let _ = watcher.unwatch(root);
            }
            for root in &c.roots {
                let _ = watcher.watch(root, RecursiveMode::Recursive);
            }
            watched = c.roots.clone();
            last = Instant::now() - Duration::from_secs(60);
        }
        if !c.paused && !gated(&c) && last.elapsed() >= Duration::from_secs(2) {
            let report = index::scan(&p, &c)?;
            if let Err(e) = index::refresh_readers(&p, &c) {
                atomic_write(&p.state.join("reader-error"), e.to_string().as_bytes())?;
            }
            atomic_write(&p.state.join("index-status.json"), &serde_json::to_vec(&report)?)?;
            if c.semantic {
                match semantic::index(&p) {
                    Ok(()) => {
                        let _ = fs::remove_file(p.state.join("semantic-error"));
                    }
                    Err(e) => atomic_write(&p.state.join("semantic-error"), e.to_string().as_bytes())?,
                }
            }
            last = Instant::now();
        }
        match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(()) => {
                std::thread::sleep(Duration::from_millis(300));
                while rx.try_recv().is_ok() {}
            }
            Err(mpsc::RecvTimeoutError::Timeout) => last = Instant::now() - Duration::from_secs(60),
            Err(_) => break,
        }
    }
    Ok(())
}
fn main() {
    unsafe {
        libc::umask(0o077);
    }
    if let Err(e) = run() {
        eprintln!("{e:#}");
        std::process::exit(1);
    }
}
