use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::{
    ffi::{CString, c_char, c_void},
    io::{BufRead, Read, Write},
    path::PathBuf,
};
unsafe extern "C" {
    fn glimpse_open(
        text: *const c_char,
        media: *const c_char,
        backends: *const c_char,
        device: *const c_char,
        error: *mut c_char,
        capacity: usize,
    ) -> *mut c_void;
    fn glimpse_close(engine: *mut c_void);
    fn glimpse_text(engine: *mut c_void, text: *const c_char, out: *mut f32, error: *mut c_char, capacity: usize) -> i32;
    fn glimpse_media(engine: *mut c_void, path: *const c_char, out: *mut f32, error: *mut c_char, capacity: usize) -> i32;
}
struct Engine(*mut c_void);
impl Drop for Engine {
    fn drop(&mut self) {
        unsafe {
            glimpse_close(self.0);
        }
    }
}
fn run() -> Result<()> {
    let models = PathBuf::from(std::env::var("GLIMPSE_MODELS")?);
    let backends = CString::new(std::env::var("GLIMPSE_GGML_DIR")?)?;
    let text = CString::new(models.join("embeddinggemma-2-Q8_0.gguf").to_str().context("model path must be UTF-8")?)?;
    let projector = if std::env::args().any(|s| s == "--text-only") {
        CString::new("")?
    } else {
        CString::new(models.join("mmproj-embeddinggemma-2-Q8_0.gguf").to_str().context("model path must be UTF-8")?)?
    };
    let device = CString::new(std::env::var("GLIMPSE_DEVICE").unwrap_or_else(|_| "cpu".into()))?;
    let mut error = [0u8; 1024];
    let pointer = unsafe {
        glimpse_open(
            text.as_ptr(),
            projector.as_ptr(),
            backends.as_ptr(),
            device.as_ptr(),
            error.as_mut_ptr().cast(),
            error.len(),
        )
    };
    if pointer.is_null() {
        bail!("{}", String::from_utf8_lossy(error.split(|b| *b == 0).next().unwrap()));
    }
    let engine = Engine(pointer);
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{}", json!({"ready":true,"dim":768,"identity":"glimpse-embeddinggemma2-q8-v1"}))?;
    stdout.flush()?;
    let mut stdin = std::io::stdin().lock();
    loop {
        let mut line = Vec::new();
        let n = stdin.by_ref().take(65537).read_until(b'\n', &mut line)?;
        if n == 0 {
            break;
        }
        if n > 65536 {
            bail!("request too large");
        }
        let request: Value = serde_json::from_slice(&line)?;
        if request["op"] == "quit" {
            break;
        }
        let result = (|| -> Result<Vec<Vec<f32>>> {
            let op = request["op"].as_str().context("missing embedding operation")?;
            let inputs = request["inputs"].as_array().context("inputs missing")?;
            if inputs.len() > 16 {
                bail!("batch too large");
            }
            let mut result = Vec::new();
            for input in inputs {
                let input = CString::new(input.as_str().context("input must be text")?)?;
                let mut vector = vec![0.; 768];
                error.fill(0);
                let rc = match op {
                    "text" => unsafe {
                        glimpse_text(engine.0, input.as_ptr(), vector.as_mut_ptr(), error.as_mut_ptr().cast(), error.len())
                    },
                    "image" | "audio" => unsafe {
                        glimpse_media(engine.0, input.as_ptr(), vector.as_mut_ptr(), error.as_mut_ptr().cast(), error.len())
                    },
                    _ => bail!("unsupported operation"),
                };
                if rc != 0 {
                    bail!("{}", String::from_utf8_lossy(error.split(|b| *b == 0).next().unwrap()));
                }
                result.push(vector);
            }
            Ok(result)
        })();
        let reply = match result {
            Ok(v) => json!({"id":request["id"],"vectors":v}),
            Err(e) => json!({"id":request["id"],"error":e.to_string()}),
        };
        serde_json::to_writer(&mut stdout, &reply)?;
        stdout.write_all(b"\n")?;
        stdout.flush()?;
    }
    Ok(())
}
fn main() {
    if let Err(e) = run() {
        println!("{}", json!({"ready":false,"error":e.to_string()}));
        std::process::exit(1);
    }
}
