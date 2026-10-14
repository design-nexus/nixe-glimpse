use anyhow::{Result, bail};
use std::os::unix::process::CommandExt;
use std::{
    io::{Read, Write},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};
/// Runs an expensive check (reading settings or plugin manifests) at most once per
/// `every`, for loops that poll cancellation every few milliseconds. Once it reports
/// true it stays true.
pub fn throttled(every: Duration, check: impl Fn() -> bool) -> impl Fn() -> bool {
    let last = std::cell::Cell::new(None::<Instant>);
    let cached = std::cell::Cell::new(false);
    move || {
        if !cached.get() && last.get().is_none_or(|t| t.elapsed() >= every) {
            cached.set(check());
            last.set(Some(Instant::now()));
        }
        cached.get()
    }
}
/// Bounded process capture. Kill the entire process group on timeout/output excess.
pub fn capture(cmd: Command, input: Vec<u8>, timeout: Duration, limit: usize) -> Result<Vec<u8>> {
    capture_cancel(cmd, input, timeout, limit, || false)
}
pub fn capture_cancel(
    mut cmd: Command,
    input: Vec<u8>,
    timeout: Duration,
    limit: usize,
    cancelled: impl Fn() -> bool,
) -> Result<Vec<u8>> {
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).process_group(0);
    let mut child = cmd.spawn()?;
    let pid = child.id() as i32;
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let result = (|| {
            let mut out = Vec::new();
            stdout.by_ref().take(limit as u64 + 1).read_to_end(&mut out)?;
            Ok::<_, std::io::Error>(out)
        })();
        let _ = tx.send(result);
    });
    thread::spawn(move || {
        let _ = stdin.write_all(&input);
    });
    let start = Instant::now();
    let result = loop {
        if cancelled() {
            break Err(anyhow::anyhow!("operation cancelled"));
        }
        if let Ok(out) = rx.try_recv() {
            break out
                .map_err(anyhow::Error::from)
                .and_then(|out| if out.len() > limit { bail!("process output exceeded limit") } else { Ok(out) });
        }
        if start.elapsed() > timeout {
            break Err(anyhow::anyhow!("process timed out"));
        }
        thread::sleep(Duration::from_millis(5));
    };
    // Always reap descendants too, including on malformed/unbounded output.
    let status = if result.is_ok() {
        loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if cancelled() || start.elapsed() > timeout {
                unsafe {
                    libc::kill(-pid, libc::SIGKILL);
                }
                break child.wait()?;
            }
            thread::sleep(Duration::from_millis(5));
        }
    } else {
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
        child.wait()?
    };
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
    match result {
        Ok(out) if status.success() => Ok(out),
        Ok(_) => bail!("process failed: {status}"),
        Err(e) => Err(e),
    }
}
