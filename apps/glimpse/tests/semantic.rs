use nixe_glimpse::{
    config::{Config, Paths},
    semantic,
};
use std::{fs, os::unix::fs::symlink, process::Command};
fn two_page_pdf() -> Vec<u8> {
    let texts = [
        "A field guide describing coastal ecosystems, migratory birds, and conservation.",
        "The quokka is a small Australian marsupial living on Rottnest Island.",
    ];
    let mut objects = vec![
        "<< /Type /Catalog /Pages 2 0 R >>".to_owned(),
        "<< /Type /Pages /Kids [4 0 R 6 0 R] /Count 2 >>".to_owned(),
        "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>".to_owned(),
    ];
    for (i, text) in texts.into_iter().enumerate() {
        let stream = format!("BT /F1 16 Tf 40 700 Td ({text}) Tj ET");
        objects.push(format!(
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Resources << /Font << /F1 3 0 R >> >> /Contents {} 0 R >>",
            5 + i * 2
        ));
        objects.push(format!("<< /Length {} >>\nstream\n{stream}\nendstream", stream.len()));
    }
    let mut pdf = String::from("%PDF-1.4\n");
    let mut positions = Vec::new();
    for (i, object) in objects.iter().enumerate() {
        positions.push(pdf.len());
        pdf.push_str(&format!("{} 0 obj\n{object}\nendobj\n", i + 1));
    }
    let offset = pdf.len();
    pdf.push_str(&format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1));
    for position in positions {
        pdf.push_str(&format!("{position:010} 00000 n \n"));
    }
    pdf.push_str(&format!("trailer\n<< /Root 1 0 R /Size {} >>\nstartxref\n{offset}\n%%EOF", objects.len() + 1));
    pdf.into_bytes()
}
#[test]
#[ignore = "requires explicitly downloaded real models and built runtime"]
fn real_multimodal_pipeline_and_scope() {
    let installed = Paths::discover();
    assert!(semantic::runtime(&installed).is_file(), "set up the runtime first");
    for (name, bytes, hash) in semantic::MODELS {
        semantic::verify(&installed.data.join("Models").join(name), bytes, hash).unwrap();
    }
    let temp = tempfile::Builder::new().prefix("glimpse-inference-").tempdir().unwrap();
    let p = Paths { config: temp.path().join("config"), data: temp.path().join("data"), state: temp.path().join("state") };
    p.ensure().unwrap();
    symlink(installed.data.join("Models"), p.data.join("Models")).unwrap();
    symlink(installed.data.join("runtime"), p.data.join("runtime")).unwrap();
    let root = temp.path().join("files");
    fs::create_dir(&root).unwrap();
    let reference = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    fs::copy(reference.join("photo.jpg"), root.join("landscape.jpg")).unwrap();
    fs::copy(reference.join("speech.wav"), root.join("recording.wav")).unwrap();
    fs::write(
        root.join("notes.md"),
        "The contract renewal is due on November 3. The legal team must approve the renewal before the deadline.",
    )
    .unwrap();
    fs::write(root.join("field.pdf"), two_page_pdf()).unwrap();
    let video = root.join("scene.mp4");
    assert!(
        Command::new("ffmpeg")
            .args(["-v", "error", "-loop", "1", "-i"])
            .arg(root.join("landscape.jpg"))
            .args(["-t", "2", "-vf", "scale=512:384", "-pix_fmt", "yuv420p", "-y"])
            .arg(&video)
            .status()
            .unwrap()
            .success()
    );
    let mut config = Config { roots: vec![root.clone()], semantic: true, battery_pause: false, ..Config::default() };
    p.save(&config).unwrap();
    semantic::index(&p).unwrap();
    let mut session = semantic::Session::start(&p).unwrap();
    let notes = session.search(&p, "contract renewal deadline", || false).unwrap();
    assert!(notes.iter().any(|v| v["path"].as_str().unwrap().ends_with("notes.md") && v.get("cosine").is_some()), "{notes:?}");
    let pdf = session.search(&p, "quokka", || false).unwrap();
    assert!(pdf.iter().any(|v| v["path"].as_str().unwrap().ends_with("field.pdf") && v["loc"].as_f64() == Some(2.)), "{pdf:?}");
    let image = session.search(&p, "image of green hills, a tree, blue sky and a yellow sun", || false).unwrap();
    assert!(
        image.iter().any(|v| v["path"].as_str().unwrap().ends_with("landscape.jpg") && v.get("cosine").is_some()),
        "{image:?}"
    );
    let audio = session.search(&p, "audio recording of a person speaking", || false).unwrap();
    assert!(
        audio.iter().any(|v| v["path"].as_str().unwrap().ends_with("recording.wav")
            && v.get("loc").is_some()
            && v.get("cosine").is_some()),
        "{audio:?}"
    );
    let video_hits = session.search(&p, "video of a tree and green hills under a yellow sun", || false).unwrap();
    assert!(
        video_hits
            .iter()
            .any(|v| v["path"] == video.to_string_lossy().as_ref() && v.get("loc").is_some() && v.get("cosine").is_some()),
        "{video_hits:?}"
    );
    config.excluded.push(root.clone());
    p.save(&config).unwrap();
    assert!(session.search(&p, "contract renewal deadline", || false).unwrap().is_empty());
    config.roots.clear();
    config.code_roots.clear();
    p.save(&config).unwrap();
    assert!(session.search(&p, "quokka", || false).unwrap().is_empty());
}

#[test]
#[ignore = "requires installed local models; small relevance check"]
fn precise_relevance_sample() {
    let installed = Paths::discover();
    let temp = tempfile::Builder::new().prefix("glimpse-relevance-").tempdir().unwrap();
    let p = Paths { config: temp.path().join("config"), data: temp.path().join("data"), state: temp.path().join("state") };
    p.ensure().unwrap();
    symlink(installed.data.join("Models"), p.data.join("Models")).unwrap();
    symlink(installed.data.join("runtime"), p.data.join("runtime")).unwrap();
    let root = temp.path().join("files");
    fs::create_dir(&root).unwrap();
    fs::write(
        root.join("agreement.txt"),
        "The contract renewal is due on November 3. The legal team must approve the renewal before the deadline.",
    )
    .unwrap();
    fs::write(
        root.join("cooking.txt"),
        "A banana bread recipe uses ripe bananas, flour, butter, and eggs. Bake the loaf in an oven until golden brown.",
    )
    .unwrap();
    p.save(&Config { roots: vec![root], semantic: true, device: "cpu".into(), ..Config::default() }).unwrap();
    semantic::index(&p).unwrap();
    let mut session = semantic::Session::start(&p).unwrap();
    let hits = session.search(&p, "legal agreement extension due date", || false).unwrap();
    eprintln!("Relevance sample: {hits:?}");
    assert!(hits.iter().any(|h| h["path"].as_str().unwrap().ends_with("agreement.txt")));
    assert!(!hits.iter().any(|h| h["path"].as_str().unwrap().ends_with("cooking.txt")));
    let unrelated = session.search(&p, "spacecraft launch orbital mechanics", || false).unwrap();
    assert!(unrelated.is_empty(), "Unrelated suggestions: {unrelated:?}");
}
