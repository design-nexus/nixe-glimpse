use crate::{
    config::{self, Paths},
    process,
};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs, io::Read, path::PathBuf, process::Command, time::Duration};
pub const REVISION: &str = "bfcd298762cc34d0357ece5ebdd31791a3a374d8";
pub const MODELS: [(&str, u64, &str); 2] = [
    ("embeddinggemma-2-Q8_0.gguf", 309855456, "2188ac1deca4b77dffefd603c2776a9d76d9d74ec01841392982ebb840b09135"),
    ("mmproj-embeddinggemma-2-Q8_0.gguf", 554821024, "c4a8a52691ecef40618438928bdf9e68379b854e24166f292592353db0aab64f"),
];
pub fn runtime(p: &Paths) -> PathBuf {
    p.data.join("runtime/bin/glimpse-embed")
}
pub fn status(p: &Paths) -> Value {
    json!({"runtime":runtime(p).is_file(),"models":MODELS.iter().map(|(name,bytes,_)|json!({"name":name,"bytes":bytes,"present":p.data.join("Models").join(name).metadata().is_ok_and(|m|m.len()==*bytes)})).collect::<Vec<_>>(),"enabled":p.load().is_ok_and(|c|c.semantic)})
}
pub fn verify(path: &std::path::Path, bytes: u64, hash: &str) -> Result<()> {
    let mut f = fs::File::open(path)?;
    if f.metadata()?.len() != bytes {
        bail!("model length mismatch");
    }
    let mut digest = Sha256::new();
    let mut buf = vec![0; 8 * 1024 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        digest.update(&buf[..n]);
    }
    if format!("{:x}", digest.finalize()) != hash {
        bail!("model checksum mismatch");
    }
    Ok(())
}
pub fn download(p: &Paths, text_only: bool) -> Result<()> {
    download_cancel(p, text_only, || false)
}
pub fn download_cancel(p: &Paths, text_only: bool, cancelled: impl Fn() -> bool) -> Result<()> {
    p.ensure()?;
    use std::os::fd::AsRawFd;
    let lock = fs::OpenOptions::new().create(true).truncate(false).write(true).open(p.state.join("model-download.lock"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        bail!("model download already in progress");
    }
    let dir = p.data.join("Models");
    config::private_dir(&dir)?;
    for (name, bytes, hash) in MODELS.into_iter().take(if text_only { 1 } else { 2 }) {
        if cancelled() {
            bail!("operation cancelled");
        }
        let target = dir.join(name);
        if target.exists() {
            let stamp = model_stamp(p, name)?;
            if fs::read_to_string(p.state.join("verified-models").join(name)).ok().as_deref() != Some(&stamp) {
                verify(&target, bytes, hash)?;
                record_verified(p, name)?;
            }
            continue;
        }
        let temp = dir.join(format!("{name}.partial"));
        let url = format!("https://huggingface.co/ggml-org/embeddinggemma-2-GGUF/resolve/{REVISION}/{name}");
        let mut cmd = Command::new("curl");
        cmd.args(["-fL", "--retry", "2", "--connect-timeout", "20", "--max-time", "1800", "--output"]).arg(&temp).arg(url);
        let result = process::capture_cancel(cmd, vec![], Duration::from_secs(1810), 65536, &cancelled)
            .and_then(|_| verify(&temp, bytes, hash));
        if let Err(e) = result {
            let _ = fs::remove_file(temp);
            return Err(e);
        }
        fs::rename(temp, target)?;
        record_verified(p, name)?;
    }
    Ok(())
}
fn model_stamp(p: &Paths, name: &str) -> Result<String> {
    let metadata = p.data.join("Models").join(name).metadata()?;
    Ok(format!("{}:{}", metadata.len(), metadata.modified()?.duration_since(std::time::UNIX_EPOCH)?.as_nanos()))
}
fn record_verified(p: &Paths, name: &str) -> Result<()> {
    config::atomic_write(&p.state.join("verified-models").join(name), model_stamp(p, name)?.as_bytes())
}
fn ensure_models(p: &Paths, text_only: bool) -> Result<()> {
    for (name, bytes, hash) in MODELS.into_iter().take(if text_only { 1 } else { 2 }) {
        let stamp = model_stamp(p, name).context("models unavailable; use Glimpse Settings to download them")?;
        if fs::read_to_string(p.state.join("verified-models").join(name)).ok().as_deref() != Some(&stamp) {
            verify(&p.data.join("Models").join(name), bytes, hash)?;
            record_verified(p, name)?;
        }
    }
    Ok(())
}
fn command(p: &Paths) -> Result<Command> {
    if !runtime(p).is_file() {
        bail!("semantic runtime unavailable; run nixe-glimpse models setup");
    }
    let mut c = Command::new(runtime(p));
    c.env("GLIMPSE_MODELS", p.data.join("Models")).env("GLIMPSE_GGML_DIR", p.data.join("runtime/lib"));
    Ok(c)
}

pub const IDENTITY: &str = "glimpse-embeddinggemma2-q8-v1";
pub struct Session {
    worker: Option<Worker>,
    last_use: std::time::Instant,
    /// Generic queries' vectors per task (search, code), for measuring each item's hubness.
    probes: std::collections::HashMap<bool, Vec<Vec<f32>>>,
}
/// Queries that ask for nothing in particular. An item that scores well against these
/// scores well against everything (a near-blank image, a boilerplate page), so its
/// similarity to them is subtracted before it can count as related.
/// They name no kind of file, so a query that asks for pictures or recordings isn't
/// marked down for it.
const PROBES: [&str; 8] = ["something", "anything", "information", "a file", "an item", "data", "stuff", "content"];
struct Worker {
    child: std::process::Child,
    input: std::process::ChildStdin,
    replies: std::sync::mpsc::Receiver<Result<Value>>,
    next_id: u64,
}
impl Drop for Worker {
    fn drop(&mut self) {
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        let _ = self.child.wait();
    }
}
impl Worker {
    fn start(p: &Paths, text_only: bool) -> Result<Self> {
        ensure_models(p, text_only)?;
        let cfg = p.load()?;
        let wanted = if text_only || cfg.device == "cpu" {
            "cpu"
        } else if cfg.device == "auto" {
            if p.data.join("runtime/lib/libggml-cuda.so").exists() {
                "cuda"
            } else if p.data.join("runtime/lib/libggml-vulkan.so").exists() {
                "vulkan"
            } else {
                "cpu"
            }
        } else {
            &cfg.device
        };
        match Self::spawn(p, text_only, wanted) {
            Ok(worker) => Ok(worker),
            Err(_) if cfg.device == "auto" && wanted != "cpu" => Self::spawn(p, text_only, "cpu"),
            Err(e) => Err(e),
        }
    }
    fn spawn(p: &Paths, text_only: bool, device: &str) -> Result<Self> {
        use std::{
            io::{BufRead, BufReader},
            os::unix::process::CommandExt,
            process::Stdio,
        };
        let mut cmd = command(p)?;
        cmd.env("GLIMPSE_DEVICE", device);
        if text_only {
            cmd.arg("--text-only");
        }
        cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).process_group(0);
        let mut child = cmd.spawn()?;
        let input = child.stdin.take().unwrap();
        let output = child.stdout.take().unwrap();
        let (tx, replies) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(output);
            loop {
                let mut bytes = Vec::new();
                match reader.by_ref().take(2 * 1024 * 1024 + 1).read_until(b'\n', &mut bytes) {
                    Ok(0) | Err(_) => break,
                    Ok(n) if n > 2 * 1024 * 1024 => {
                        let _ = tx.send(Err(anyhow::anyhow!("inference output exceeded limit")));
                        break;
                    }
                    Ok(_) => {
                        if tx.send(serde_json::from_slice(&bytes).map_err(anyhow::Error::from)).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        let worker = Self { child, input, replies, next_id: 0 };
        let ready = worker.replies.recv_timeout(Duration::from_secs(30)).context("inference startup timed out")??;
        if ready["ready"] != true || ready["dim"] != 768 || ready["identity"] != IDENTITY {
            bail!("inference unavailable or incompatible: {}", ready["error"]);
        }
        Ok(worker)
    }
    fn embed(&mut self, op: &str, input: &str, cancelled: &impl Fn() -> bool) -> Result<Vec<f32>> {
        use std::io::Write;
        self.next_id += 1;
        let id = self.next_id;
        let request = json!({"id":id,"op":op,"inputs":[input]});
        serde_json::to_writer(&mut self.input, &request)?;
        self.input.write_all(b"\n")?;
        self.input.flush()?;
        let start = std::time::Instant::now();
        loop {
            if cancelled() {
                bail!("inference cancelled");
            }
            if start.elapsed() > Duration::from_secs(30) {
                bail!("inference timed out");
            }
            match self.replies.recv_timeout(Duration::from_millis(25)) {
                Ok(v) => {
                    let v = v?;
                    if v["id"] != id {
                        continue;
                    }
                    if let Some(e) = v.get("error") {
                        bail!("inference: {e}");
                    }
                    let vector: Vec<f32> = serde_json::from_value(v["vectors"][0].clone())?;
                    validate_vector(&vector)?;
                    return Ok(vector);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => bail!("inference worker exited"),
                Err(_) => {}
            }
        }
    }
}
pub fn validate_vector(vector: &[f32]) -> Result<()> {
    if vector.len() != 768 || vector.iter().any(|n| !n.is_finite()) {
        bail!("invalid embedding shape or values");
    }
    let norm: f32 = vector.iter().map(|n| n * n).sum();
    if !(0.99..=1.01).contains(&norm) {
        bail!("embedding is not normalized");
    }
    Ok(())
}
fn database(p: &Paths) -> Result<rusqlite::Connection> {
    use std::os::unix::fs::PermissionsExt;
    p.ensure()?;
    let dir = p.data.join("semantic-index");
    config::private_dir(&dir)?;
    let file = dir.join("glimpse.sqlite");
    if file.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()) {
        bail!("semantic index is a symlink");
    }
    let db = rusqlite::Connection::open(&file)?;
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600))?;
    db.busy_timeout(Duration::from_secs(3))?;
    db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA recursive_triggers=ON; CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY,value TEXT); CREATE TABLE IF NOT EXISTS documents(path TEXT PRIMARY KEY,stamp TEXT NOT NULL,status TEXT NOT NULL,cursor INTEGER NOT NULL DEFAULT 0,error TEXT); CREATE TABLE IF NOT EXISTS segments(id INTEGER PRIMARY KEY,path TEXT NOT NULL,ordinal INTEGER NOT NULL,kind TEXT NOT NULL,loc REAL,text TEXT NOT NULL,vector BLOB NOT NULL,code INTEGER NOT NULL DEFAULT 0,reader TEXT, UNIQUE(path,ordinal)); CREATE VIRTUAL TABLE IF NOT EXISTS words USING fts5(text); CREATE TRIGGER IF NOT EXISTS segment_add AFTER INSERT ON segments BEGIN INSERT INTO words(rowid,text) VALUES(new.id,new.text); END; CREATE TRIGGER IF NOT EXISTS segment_del AFTER DELETE ON segments BEGIN DELETE FROM words WHERE rowid=old.id; END;")?;
    let identity: Option<String> = db.query_row("SELECT value FROM meta WHERE key='identity'", [], |r| r.get(0)).ok();
    if identity.as_deref() != Some(IDENTITY) {
        db.execute_batch("DELETE FROM segments;DELETE FROM documents;")?;
        db.execute("INSERT OR REPLACE INTO meta(key,value) VALUES('identity',?1)", [IDENTITY])?;
    }
    Ok(db)
}
fn pack(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|n| n.to_le_bytes()).collect()
}
fn unpack(bytes: &[u8]) -> Result<Vec<f32>> {
    if bytes.len() != 768 * 4 {
        bail!("corrupt stored vector");
    }
    let v = bytes.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect::<Vec<_>>();
    validate_vector(&v)?;
    Ok(v)
}
impl Session {
    pub fn start(_p: &Paths) -> Result<Self> {
        Ok(Self { worker: None, last_use: std::time::Instant::now(), probes: Default::default() })
    }
    pub fn release_idle(&mut self) {
        if self.last_use.elapsed() > Duration::from_secs(30) {
            self.worker = None;
        }
    }
    pub fn search(&mut self, p: &Paths, q: &str, cancelled: impl Fn() -> bool) -> Result<Vec<Value>> {
        let cfg = p.load()?;
        if !cfg.semantic || q.trim().is_empty() || cfg.roots.is_empty() {
            return Ok(vec![]);
        }
        if !runtime(p).exists() {
            bail!("semantic runtime unavailable; set it up in Glimpse Settings");
        }
        let db = database(p)?;
        let count: i64 = db.query_row("SELECT count(*) FROM segments", [], |r| r.get(0))?;
        if count == 0 {
            return Ok(vec![]);
        }
        self.release_idle();
        if self.worker.is_none() {
            self.worker = Some(Worker::start(p, true)?);
        }
        let code = q.trim().starts_with("code:");
        let query = if code { q.trim()[5..].trim() } else { q.trim().strip_prefix("semantic:").unwrap_or(q.trim()).trim() };
        let prompt = format!("task: {} | query: {query}", if code { "code retrieval" } else { "search result" });
        let vector = match self.worker.as_mut().unwrap().embed("text", &prompt, &cancelled) {
            Ok(v) => v,
            Err(e) => {
                self.worker = None;
                return Err(e);
            }
        };
        if !self.probes.contains_key(&code) {
            let task = if code { "code retrieval" } else { "search result" };
            let mut vectors = Vec::new();
            for probe in PROBES {
                let prompt = format!("task: {task} | query: {probe}");
                match self.worker.as_mut().unwrap().embed("text", &prompt, &cancelled) {
                    Ok(v) => vectors.push(v),
                    Err(e) => {
                        self.worker = None;
                        return Err(e);
                    }
                }
            }
            self.probes.insert(code, vectors);
        }
        let probes = &self.probes[&code];
        self.last_use = std::time::Instant::now();
        let active = crate::plugins::list(p)?;
        let permitted = |path: &str, reader: &Option<String>, is_code: bool| {
            config::allowed(&cfg, std::path::Path::new(path))
                && (!is_code || cfg.code_roots.iter().any(|r| std::path::Path::new(path).starts_with(r)))
                && reader.as_ref().is_none_or(|id| {
                    active.iter().any(|plugin| {
                        plugin.manifest.id == *id
                            && plugin.grant.enabled
                            && plugin.grant.permissions.read.iter().any(|r| std::path::Path::new(path).starts_with(r))
                    })
                })
        };
        let mut stmt = db.prepare("SELECT id,path,kind,loc,text,vector,code,reader FROM segments WHERE code=?1")?;
        // Best segment per file by lift: its similarity to the query minus its similarity to
        // queries for nothing in particular. Raw cosine favours "hubs" (tiny theme images,
        // boilerplate pages) that sit close to every query and turned up for everything.
        let mut best = std::collections::HashMap::<String, (f32, Value)>::new();
        for row in stmt.query_map([code], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<f64>>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Vec<u8>>(5)?,
                r.get::<_, bool>(6)?,
                r.get::<_, Option<String>>(7)?,
            ))
        })? {
            if cancelled() {
                bail!("search cancelled");
            }
            let (id, path, kind, loc, text, bytes, is_code, reader) = row?;
            if !permitted(&path, &reader, is_code) {
                continue;
            }
            let stored = unpack(&bytes)?;
            let dot = |q: &[f32]| q.iter().zip(&stored).map(|(a, b)| a * b).sum::<f32>();
            let cosine = dot(&vector);
            let baseline = probes.iter().map(|p| dot(p)).sum::<f32>() / probes.len() as f32;
            let lift = cosine - baseline;
            if best.get(&path).is_none_or(|(score, _)| lift > *score) {
                let file_kind = match kind.as_str() {
                    "page" | "page_image" => "pdf",
                    "image" => "image",
                    "audio" => "audio",
                    "frame" => "video",
                    _ => "doc",
                };
                best.insert(path.clone(),(lift,json!({"path":path,"kind":file_kind,"segment":kind,"loc":loc,"cosine":cosine,"lift":lift,"segment_id":id,"excerpt":text.chars().take(1000).collect::<String>(),"reader":reader})));
            }
        }
        // Clear standouts only: on this model, related files lift 0.09–0.2 above the generic
        // baseline while unrelated ones stay under about 0.07.
        // Clear standouts only. On this model related files lift 0.09–0.2 above the generic
        // baseline and unrelated ones stay under about 0.07. Sound matches text weakly, so a
        // single result far ahead of every other (0.06 or more) also counts.
        let mut ranked: Vec<_> = best.into_values().collect();
        ranked.sort_by(|a, b| b.0.total_cmp(&a.0));
        let standout = match ranked.as_slice() {
            [(top, _), (next, _), ..] => *top > 0. && top - next >= 0.06,
            [(top, _)] => *top > 0.,
            [] => false,
        };
        let mut semantic: Vec<_> =
            ranked.into_iter().enumerate().filter(|(i, (lift, _))| *lift >= 0.09 || (*i == 0 && standout)).map(|(_, hit)| hit).collect();
        if let Some((top, _)) = semantic.first() {
            let minimum = *top - 0.05;
            semantic.retain(|(lift, _)| *lift >= minimum);
        }
        let mut fused = std::collections::HashMap::<String, (f64, Value)>::new();
        for (rank, (_, mut hit)) in semantic.into_iter().take(5).enumerate() {
            hit["related"] = true.into();
            fused.insert(hit["path"].as_str().unwrap().into(), (1. / (60. + rank as f64), hit));
        }
        // Words in a file's contents count only when they appear together, as typed, not
        // scattered across a 1,500-character chunk; prefixes are for names, not contents.
        let expression = crate::query::fts_near(query);
        if !expression.is_empty() {
            let mut words=db.prepare("SELECT s.path,s.kind,s.loc,s.text,s.reader,s.code FROM words JOIN segments s ON s.id=words.rowid WHERE words MATCH ?1 AND s.code=?2 ORDER BY bm25(words) LIMIT 100")?;
            let mut seen = std::collections::HashSet::new();
            let mut rank = 0;
            for row in words.query_map(rusqlite::params![expression, code], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, Option<f64>>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, bool>(5)?,
                ))
            })? {
                let (path, kind, loc, text, reader, is_code) = row?;
                if !permitted(&path, &reader, is_code) || !seen.insert(path.clone()) {
                    continue;
                }
                if seen.len() > 8 {
                    break;
                }
                let entry=fused.entry(path.clone()).or_insert_with(||(0.,json!({"path":path,"kind":if kind=="page"{"pdf"}else{"doc"},"segment":kind,"loc":loc,"excerpt":text.chars().take(1000).collect::<String>(),"reader":reader})));
                entry.0 += 1. / (60. + rank as f64);
                entry.1["keyword_rank"] = rank.into();
                entry.1["related"] = false.into();
                if rank == 0 && (kind == "page" || kind == "lines") {
                    entry.1["loc"] = json!(loc);
                }
                rank += 1;
            }
        }
        let current = p.load()?;
        let mut hits: Vec<_> = fused
            .into_values()
            .filter(|(_, v)| v["path"].as_str().is_some_and(|path| config::allowed(&current, std::path::Path::new(path))))
            .collect();
        hits.sort_by(|a, b| b.0.total_cmp(&a.0));
        Ok(hits
            .into_iter()
            .take(20)
            .map(|(score, mut v)| {
                v["score"] = score.into();
                v
            })
            .collect())
    }
}
pub fn search(p: &Paths, q: &str) -> Result<Vec<Value>> {
    Session::start(p)?.search(p, q, || false)
}
struct Piece {
    ordinal: usize,
    op: &'static str,
    input: String,
    text: String,
    kind: &'static str,
    loc: Option<f64>,
    reader: Option<String>,
}
fn extract(
    p: &Paths,
    path: &std::path::Path,
    c: &crate::config::Config,
    cursor: usize,
) -> Result<(Vec<Piece>, bool, bool, usize)> {
    let ext = path.extension().unwrap_or_default().to_string_lossy().to_lowercase();
    let code = c.code_roots.iter().any(|r| path.starts_with(r))
        && ["rs", "py", "js", "ts", "tsx", "jsx", "c", "cpp", "h", "go", "java", "sh", "swift", "qml", "lua", "rb"]
            .contains(&ext.as_str());
    let mut pieces = Vec::new();
    let scratch = p.state.join("extract");
    config::private_dir(&scratch)?;
    let capture = |program: &str, args: Vec<std::ffi::OsString>, limit: usize| -> Result<Vec<u8>> {
        let mut command = Command::new(program);
        command.args(args);
        let stop = process::throttled(Duration::from_millis(250), || {
            p.load().map_or(true, |now| now.paused || !config::allowed(&now, path))
        });
        process::capture_cancel(command, vec![], Duration::from_secs(10), limit, stop)
    };
    if ext == "pdf" {
        let bytes = capture("pdfinfo", vec![path.as_os_str().to_owned()], 65536)?;
        let info = String::from_utf8_lossy(&bytes);
        let total = info
            .lines()
            .find_map(|s| s.strip_prefix("Pages:").and_then(|s| s.trim().parse::<usize>().ok()))
            .context("PDF page count unavailable")?
            .min(5000);
        for page in (cursor + 1)..=(cursor + 8).min(total) {
            let text = String::from_utf8(capture(
                "pdftotext",
                vec![
                    "-f".into(),
                    page.to_string().into(),
                    "-l".into(),
                    page.to_string().into(),
                    path.as_os_str().to_owned(),
                    "-".into(),
                ],
                256 * 1024,
            )?)?;
            if text.chars().filter(|x| !x.is_whitespace()).count() >= 40 {
                let chars: Vec<char> = text.chars().collect();
                for (chunk_number, chunk) in chars.chunks(1500).enumerate() {
                    let text: String = chunk.iter().collect();
                    pieces.push(Piece {
                        ordinal: page * 10000 + chunk_number,
                        op: "text",
                        input: format!("title: none | text: {text}"),
                        text,
                        kind: "page",
                        loc: Some(page as f64),
                        reader: None,
                    });
                }
            } else {
                let output = scratch.join(format!("page-{page}"));
                capture(
                    "pdftoppm",
                    vec![
                        "-f".into(),
                        page.to_string().into(),
                        "-l".into(),
                        page.to_string().into(),
                        "-singlefile".into(),
                        "-scale-to".into(),
                        "768".into(),
                        "-png".into(),
                        path.as_os_str().to_owned(),
                        output.as_os_str().to_owned(),
                    ],
                    65536,
                )?;
                pieces.push(Piece {
                    ordinal: page * 10000,
                    op: "image",
                    input: output.with_extension("png").to_string_lossy().into_owned(),
                    text,
                    kind: "page_image",
                    loc: Some(page as f64),
                    reader: None,
                });
            }
        }
        return Ok((pieces, cursor + 8 >= total, false, (cursor + 8).min(total)));
    }
    if ["jpg", "jpeg", "png", "webp", "heic", "gif", "bmp", "tiff", "tif"].contains(&ext.as_str()) {
        let output = scratch.join("image.png");
        capture(
            "ffmpeg",
            vec![
                "-v".into(),
                "error".into(),
                "-protocol_whitelist".into(),
                "file,pipe".into(),
                "-i".into(),
                path.as_os_str().to_owned(),
                "-frames:v".into(),
                "1".into(),
                "-vf".into(),
                "scale=768:768:force_original_aspect_ratio=decrease".into(),
                "-y".into(),
                output.as_os_str().to_owned(),
            ],
            65536,
        )?;
        pieces.push(Piece {
            ordinal: 0,
            op: "image",
            input: output.to_string_lossy().into_owned(),
            text: String::new(),
            kind: "image",
            loc: None,
            reader: None,
        });
        if crate::plugins::on_path("tesseract")
            && let Ok(bytes) = capture("tesseract", vec![output.as_os_str().to_owned(), "stdout".into()], 65536)
        {
            let text = String::from_utf8_lossy(&bytes).into_owned();
            if !text.trim().is_empty() {
                pieces.push(Piece {
                    ordinal: 1,
                    op: "text",
                    input: format!("title: none | text: {}", text.chars().take(1500).collect::<String>()),
                    text,
                    kind: "ocr",
                    loc: None,
                    reader: None,
                });
            }
        }
        return Ok((pieces, true, false, 1));
    }
    if ["wav", "mp3", "flac", "ogg", "m4a", "aac", "mp4", "mkv", "webm", "mov", "avi"].contains(&ext.as_str()) {
        let video = ["mp4", "mkv", "webm", "mov", "avi"].contains(&ext.as_str());
        let bytes = capture(
            "ffprobe",
            vec![
                "-v".into(),
                "error".into(),
                "-protocol_whitelist".into(),
                "file,pipe".into(),
                "-show_format".into(),
                "-show_streams".into(),
                "-of".into(),
                "json".into(),
                path.as_os_str().to_owned(),
            ],
            65536,
        )?;
        let meta: Value = serde_json::from_slice(&bytes)?;
        let duration = meta["format"]["duration"]
            .as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|n| n.is_finite() && *n > 0.)
            .context("media duration unavailable")?;
        let has_audio = meta["streams"].as_array().is_some_and(|s| s.iter().any(|s| s["codec_type"] == "audio"));
        let step = if video { 3. } else { 25. };
        let total = (duration / step).ceil() as usize;
        for part in cursor..(cursor + 60).min(total) {
            let seconds = part as f64 * step;
            let output = scratch.join(if video { format!("frame-{part}.png") } else { format!("audio-{part}.wav") });
            let mut args: Vec<std::ffi::OsString> = vec![
                "-v".into(),
                "error".into(),
                "-protocol_whitelist".into(),
                "file,pipe".into(),
                "-ss".into(),
                seconds.to_string().into(),
                "-i".into(),
                path.as_os_str().to_owned(),
            ];
            if video {
                args.extend([
                    "-frames:v".into(),
                    "1".into(),
                    "-vf".into(),
                    "scale=768:768:force_original_aspect_ratio=decrease".into(),
                    "-f".into(),
                    "image2".into(),
                    "-vcodec".into(),
                    "png".into(),
                ]);
            } else {
                args.extend([
                    "-t".into(),
                    "30".into(),
                    "-ac".into(),
                    "1".into(),
                    "-ar".into(),
                    "16000".into(),
                    "-f".into(),
                    "wav".into(),
                ]);
            }
            args.extend(["-y".into(), output.as_os_str().to_owned()]);
            capture("ffmpeg", args, 65536)?;
            pieces.push(Piece {
                ordinal: part * 2,
                op: if video { "image" } else { "audio" },
                input: output.to_string_lossy().into_owned(),
                text: String::new(),
                kind: if video { "frame" } else { "audio" },
                loc: Some(seconds),
                reader: None,
            });
            if video && has_audio && part % 8 == 0 {
                let output = scratch.join(format!("track-{part}.wav"));
                capture(
                    "ffmpeg",
                    vec![
                        "-v".into(),
                        "error".into(),
                        "-protocol_whitelist".into(),
                        "file,pipe".into(),
                        "-ss".into(),
                        seconds.to_string().into(),
                        "-i".into(),
                        path.as_os_str().to_owned(),
                        "-t".into(),
                        "30".into(),
                        "-ac".into(),
                        "1".into(),
                        "-ar".into(),
                        "16000".into(),
                        "-y".into(),
                        output.as_os_str().to_owned(),
                    ],
                    65536,
                )?;
                pieces.push(Piece {
                    ordinal: part * 2 + 1,
                    op: "audio",
                    input: output.to_string_lossy().into_owned(),
                    text: String::new(),
                    kind: "audio",
                    loc: Some(seconds),
                    reader: None,
                });
            }
        }
        return Ok((pieces, cursor + 60 >= total, false, (cursor + 60).min(total)));
    }
    if code || ["txt", "md", "rst", "csv", "html", "htm", "rtf", "docx", "odt", "pptx", "xlsx"].contains(&ext.as_str()) {
        let text = if ["docx", "odt", "pptx", "xlsx"].contains(&ext.as_str()) {
            let member = match ext.as_str() {
                "docx" => "word/document.xml",
                "odt" => "content.xml",
                "pptx" => "ppt/slides/slide*.xml",
                _ => "xl/sharedStrings.xml",
            };
            let bytes = capture("unzip", vec!["-p".into(), path.as_os_str().to_owned(), member.into()], 2 * 1024 * 1024)?;
            strip_markup(&String::from_utf8_lossy(&bytes))
        } else {
            let mut bytes = Vec::new();
            fs::File::open(path)?.take(4 * 1024 * 1024 + 1).read_to_end(&mut bytes)?;
            if bytes.len() > 4 * 1024 * 1024 {
                bail!("text file exceeds extraction limit; use a reader plugin");
            }
            // Older text files are often Latin-1; index what can be read rather than failing.
            let text = String::from_utf8_lossy(&bytes).into_owned();
            if ["html", "htm"].contains(&ext.as_str()) { strip_markup(&text) } else { text }
        };
        let chars: Vec<char> = text.chars().collect();
        let total = chars.len().div_ceil(1500);
        for part in cursor..(cursor + 128).min(total) {
            let start = part * 1500;
            let chunk: String = chars[start..(start + 1500).min(chars.len())].iter().collect();
            let line = chars[..start].iter().filter(|c| **c == '\n').count() + 1;
            pieces.push(Piece {
                ordinal: part,
                op: "text",
                input: format!("title: none | text: {chunk}"),
                text: chunk,
                kind: if code { "lines" } else { "chunk" },
                loc: if code { Some(line as f64) } else { None },
                reader: None,
            });
        }
        return Ok((pieces, cursor + 128 >= total, code, (cursor + 128).min(total)));
    }
    // Plugins add formats without changing the extractor.
    let content = crate::index::open(p)?;
    let mut rows = content.prepare("SELECT plugin,info,text FROM extracted WHERE path=?1")?;
    for row in rows.query_map([path.to_string_lossy().as_ref()], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
    })? {
        let (id, info, text) = row?;
        let plugin = crate::plugins::get(p, &id)?;
        if !plugin.grant.enabled {
            continue;
        }
        let info: Value = serde_json::from_str(&info)?;
        pieces.push(Piece {
            ordinal: pieces.len(),
            op: "text",
            input: format!("title: none | text: {}", text.chars().take(1500).collect::<String>()),
            text,
            kind: if info["page"].is_number() {
                "page"
            } else if info["line"].is_number() {
                "lines"
            } else {
                "chunk"
            },
            loc: info["page"].as_f64().or_else(|| info["line"].as_f64()).or_else(|| info["seconds"].as_f64()),
            reader: Some(id),
        });
    }
    Ok((pieces, true, false, 1))
}
/// Text of an XML/HTML document: tags become spaces and entities are decoded.
pub fn strip_markup(text: &str) -> String {
    let mut inside = false;
    let mut out = String::new();
    for ch in text.chars() {
        match ch {
            '<' => {
                inside = true;
                out.push(' ');
            }
            '>' => inside = false,
            _ if !inside => out.push(ch),
            _ => {}
        }
    }
    decode_entities(&out)
}
/// Decodes named XML entities and numeric references in one pass, so "&amp;lt;" stays "&lt;".
fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let decoded = rest[1..].find(';').filter(|end| *end <= 10).and_then(|end| {
            let name = &rest[1..=end];
            let ch = match name {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some('\u{a0}'),
                _ => name
                    .strip_prefix("#x")
                    .or_else(|| name.strip_prefix("#X"))
                    .map(|hex| u32::from_str_radix(hex, 16))
                    .or_else(|| name.strip_prefix('#').map(str::parse::<u32>))
                    .and_then(|n| n.ok())
                    .and_then(char::from_u32),
            }?;
            Some((ch, end + 2))
        });
        match decoded {
            Some((ch, len)) => {
                out.push(ch);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}
#[cfg(test)]
mod tests {
    #[test]
    fn entities_decode_once() {
        assert_eq!(super::strip_markup("<w:t>a &amp;lt; b &quot;c&apos; &#233;&#x41; &bogus; & x</w:t>"), " a &lt; b \"c' éA &bogus; & x ");
    }
}
/// Scan the sources, then index their contents.
pub fn index(p: &Paths) -> Result<()> {
    let cfg = p.load()?;
    if cfg.semantic && !cfg.paused && !cfg.roots.is_empty() {
        crate::index::scan(p, &cfg)?;
    }
    index_scanned(p).map(|_| ())
}
/// Index contents of files the filename index already lists; the daemon scans first.
/// Returns true when work is left over (time budget used up or files part-done), so the
/// caller should run another pass soon.
pub fn index_scanned(p: &Paths) -> Result<bool> {
    use std::os::fd::AsRawFd;
    p.ensure()?;
    let lock = fs::OpenOptions::new().create(true).truncate(false).write(true).open(p.state.join("semantic.lock"))?;
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Ok(false);
    }
    let cfg = p.load()?;
    if !cfg.semantic || cfg.paused || cfg.roots.is_empty() {
        return Ok(false);
    }
    let mut out_of_time = false;
    let source = crate::index::open(p)?;
    let mut db = database(p)?;
    let documents: Vec<String> =
        db.prepare("SELECT path FROM documents")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    for path in documents {
        let exists: bool = source.query_row("SELECT EXISTS(SELECT 1 FROM files WHERE path=?1)", [&path], |r| r.get(0))?;
        if !exists || !config::allowed(&cfg, std::path::Path::new(&path)) {
            db.execute("DELETE FROM segments WHERE path=?1", [&path])?;
            db.execute("DELETE FROM documents WHERE path=?1", [&path])?;
        }
    }
    let files: Vec<(String, i64, i64)> = source
        .prepare("SELECT path,mtime,size FROM files ORDER BY path")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let mut worker: Option<Worker> = None;
    let started = std::time::Instant::now();
    let reader_version = crate::plugins::list(p)?
        .into_iter()
        .filter(|x| x.grant.enabled && x.manifest.capabilities.contains(&"reader".into()))
        .map(|x| format!("{}:{}", x.manifest.id, x.manifest.version))
        .collect::<Vec<_>>()
        .join("|");
    let mut current = cfg.clone();
    let mut loaded = std::time::Instant::now();
    for (path, mtime, size) in files {
        // Settings are re-read at most once a second, not once per file.
        if loaded.elapsed() >= Duration::from_secs(1) {
            current = p.load()?;
            loaded = std::time::Instant::now();
        }
        if current.paused
            || !current.semantic
            || current.roots != cfg.roots
            || current.excluded != cfg.excluded
        {
            break;
        }
        if started.elapsed() > Duration::from_secs(300) {
            out_of_time = true;
            break;
        }
        let file = std::path::Path::new(&path);
        if !config::allowed(&current, file) || !file.is_file() {
            continue;
        }
        let stamp = format!("{mtime}:{size}:{}:{reader_version}", cfg.code_roots.iter().any(|r| file.starts_with(r)));
        let cached: Option<(String, String, i64)> = db
            .query_row("SELECT stamp,status,cursor FROM documents WHERE path=?1", [&path], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .ok();
        // A failed file is retried only once it changes, not on every pass.
        if cached.as_ref().is_some_and(|(s, state, _)| s == &stamp && (state == "done" || state == "error")) {
            continue;
        }
        let cursor = cached.as_ref().filter(|(s, _, _)| s == &stamp).map(|(_, _, n)| *n as usize).unwrap_or(0);
        if cursor == 0 {
            db.execute("DELETE FROM segments WHERE path=?1", [&path])?;
        }
        let cancelled = process::throttled(Duration::from_millis(250), || {
            p.load().map_or(true, |c| c.paused || !c.semantic || !config::allowed(&c, file))
        });
        // Only a worker failure restarts the model; a file that can't be read leaves it running.
        let mut worker_fault = false;
        let result = (|| -> Result<(usize, bool)> {
            let (pieces, complete, code, next_cursor) = extract(p, file, &current, cursor)?;
            if pieces.is_empty() {
                return Ok((next_cursor, complete));
            }
            if worker.is_none() {
                worker = Some(Worker::start(p, false).inspect_err(|_| worker_fault = true)?);
            }
            let mut ready = Vec::new();
            for piece in pieces {
                if cancelled() {
                    bail!("indexing cancelled");
                }
                let vector =
                    worker.as_mut().unwrap().embed(piece.op, &piece.input, &cancelled).inspect_err(|_| worker_fault = true)?;
                ready.push((piece, vector));
            }
            if cancelled() {
                bail!("source changed");
            }
            let meta = file.metadata()?;
            let now = meta.modified()?.duration_since(std::time::UNIX_EPOCH)?.as_secs();
            if now != mtime as u64 || meta.len() != size as u64 {
                bail!("file changed during extraction");
            }

            let tx = db.transaction()?;
            for (piece, vector) in ready {
                tx.execute("INSERT OR REPLACE INTO segments(path,ordinal,kind,loc,text,vector,code,reader) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",rusqlite::params![path,piece.ordinal as i64,piece.kind,piece.loc,piece.text,pack(&vector),code,piece.reader])?;
            }
            tx.commit()?;
            Ok((next_cursor, complete))
        })();
        match result {
            Ok((next_cursor, complete)) => {
                db.execute(
                    "INSERT OR REPLACE INTO documents(path,stamp,status,cursor,error) VALUES(?1,?2,?3,?4,NULL)",
                    rusqlite::params![path, stamp, if complete { "done" } else { "partial" }, next_cursor as i64],
                )?;
            }
            Err(e) => {
                // Paused, switched off or source removed: leave the document for a later pass.
                if cancelled() {
                    break;
                }
                // A worker crash or timeout may pass; a file that can't be read waits until it changes.
                let status = if worker_fault { "retry" } else { "error" };
                if worker_fault {
                    worker = None;
                }
                db.execute(
                    "INSERT OR REPLACE INTO documents(path,stamp,status,cursor,error) VALUES(?1,?2,?3,?4,?5)",
                    rusqlite::params![path, stamp, status, cursor as i64, e.to_string()],
                )?;
            }
        }
        config::atomic_write(
            &p.state.join("semantic-status.json"),
            &serde_json::to_vec(
                &json!({"event":"indexing","files":db.query_row("SELECT count(*) FROM documents WHERE status='done'",[],|r|r.get::<_,i64>(0))?,"errors":db.query_row("SELECT count(*) FROM documents WHERE status='error'",[],|r|r.get::<_,i64>(0))?}),
            )?,
        )?;
    }
    if p.state.join("extract").exists() {
        fs::remove_dir_all(p.state.join("extract"))?;
    }
    let partial: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM documents WHERE status='partial')", [], |r| r.get(0))?;
    Ok(out_of_time || partial)
}
