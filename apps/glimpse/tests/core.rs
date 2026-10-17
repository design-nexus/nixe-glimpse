use nixe_glimpse::{
    config::{self, Config, Paths},
    index, integration, plugins, process, semantic, utility,
};
use serde_json::json;
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::PathBuf,
    process::Command,
    time::Duration,
};
#[allow(dead_code)]
#[path = "../../settings/src/ext/protocol.rs"]
mod current_host;
fn fixture() -> (tempfile::TempDir, Paths, Config) {
    let temp = tempfile::Builder::new().prefix("glimpse-test-").tempdir().unwrap();
    let p = Paths { config: temp.path().join("config"), data: temp.path().join("data"), state: temp.path().join("state") };
    p.ensure().unwrap();
    let root = temp.path().join("source");
    fs::create_dir(&root).unwrap();
    let c = Config { roots: vec![root], ..Config::default() };
    p.save(&c).unwrap();
    (temp, p, c)
}
#[test]
fn arithmetic_and_affine_units() {
    assert_eq!(utility::calculate("2+3*(4-1)").unwrap(), 11.);
    assert_eq!(utility::calculate("2^3^2").unwrap(), 512.);
    assert_eq!(utility::calculate("-2^2").unwrap(), -4.);
    for q in ["1/0", "1+", "system('x')", "(((", &"(".repeat(40)] {
        assert!(utility::calculate(q).is_err(), "{q}");
    }
    let c = Config::default();
    assert!(utility::evaluate("32 f to c", &c).unwrap().unwrap().starts_with("0.00000000"));
    assert!(utility::evaluate("1 kg to km", &c).is_err());
    assert!(utility::evaluate("-1 k to c", &c).is_err());
}
#[test]
fn currency_requires_valid_dated_rates() {
    let mut c = Config::default();
    assert!(utility::evaluate("10 USD to EUR", &c).is_err());
    c.rates.insert("USD".into(), 1.);
    c.rates.insert("EUR".into(), 0.9);
    assert!(config::validate(&c).is_err());
    c.rates_date = "2026-10-13".into();
    assert!(config::validate(&c).is_ok());
    assert!(utility::evaluate("10 USD to EUR", &c).unwrap().unwrap().contains("9.00 EUR"));
    assert!(!config::valid_date("2026-02-29"));
    assert!(config::valid_date("2024-02-29"));
}
#[test]
fn invalid_config_does_not_overwrite() {
    let (_t, p, mut c) = fixture();
    let before = fs::read(p.config_file()).unwrap();
    c.roots.push(PathBuf::from("/"));
    assert!(p.save(&c).is_err());
    assert_eq!(fs::read(p.config_file()).unwrap(), before);
    assert_eq!(fs::metadata(p.config_file()).unwrap().permissions().mode() & 0o777, 0o600);
}
#[test]
fn index_reconciles_unicode_and_deletion() {
    let (_t, p, c) = fixture();
    let file = c.roots[0].join("O'Brien 東京 report.txt");
    fs::write(&file, "first").unwrap();
    index::scan(&p, &c).unwrap();
    assert_eq!(index::search(&p, &c, "東京", 20).unwrap().len(), 1);
    fs::write(&file, "second version").unwrap();
    index::scan(&p, &c).unwrap();
    assert_eq!(index::search(&p, &c, "report", 20).unwrap().len(), 1);
    fs::remove_file(file).unwrap();
    index::scan(&p, &c).unwrap();
    assert!(index::search(&p, &c, "report", 20).unwrap().is_empty());
}
#[test]
fn secrets_symlinks_and_removed_sources_stay_hidden() {
    let (_t, p, mut c) = fixture();
    let root = &c.roots[0];
    for name in [".env", "secret.key", "id_rsa", "report.txt"] {
        fs::write(root.join(name), "x").unwrap();
    }
    fs::create_dir(root.join("node_modules")).unwrap();
    fs::write(root.join("node_modules/library.txt"), "x").unwrap();
    symlink(root.join("report.txt"), root.join("alias.txt")).unwrap();
    index::scan(&p, &c).unwrap();
    let conn = index::open(&p).unwrap();
    let count: i64 = conn.query_row("SELECT count(*) FROM files", [], |r| r.get(0)).unwrap();
    assert_eq!(count, 1);
    c.roots.clear();
    p.save(&c).unwrap();
    assert!(index::search(&p, &c, "report", 20).unwrap().is_empty());
    index::purge_outside(&conn, &c).unwrap();
    let count: i64 = conn.query_row("SELECT count(*) FROM files", [], |r| r.get(0)).unwrap();
    assert_eq!(count, 0);
}
#[test]
fn unmounted_root_does_not_prune() {
    let (_t, p, c) = fixture();
    fs::write(c.roots[0].join("report.txt"), "x").unwrap();
    index::scan(&p, &c).unwrap();
    fs::rename(&c.roots[0], c.roots[0].with_file_name("offline")).unwrap();
    let result = index::scan(&p, &c).unwrap();
    assert_eq!(result["unavailable"].as_array().unwrap().len(), 1);
    let conn = index::open(&p).unwrap();
    let count: i64 = conn.query_row("SELECT count(*) FROM files", [], |r| r.get(0)).unwrap();
    assert_eq!(count, 1);
}
#[test]
fn unreadable_subfolder_does_not_hide_the_source() {
    let (_t, p, c) = fixture();
    fs::write(c.roots[0].join("report.txt"), "x").unwrap();
    let locked = c.roots[0].join("locked");
    fs::create_dir(&locked).unwrap();
    fs::write(locked.join("inside.txt"), "x").unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
    let result = index::scan(&p, &c);
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(result.unwrap()["unavailable"].as_array().unwrap().is_empty());
    assert_eq!(index::search(&p, &c, "report", 20).unwrap().len(), 1);
}
#[test]
fn failed_documents_wait_until_they_change() {
    let (_t, p, mut c) = fixture();
    let broken = c.roots[0].join("empty.docx");
    fs::write(&broken, "").unwrap();
    c.semantic = true;
    c.battery_pause = false;
    p.save(&c).unwrap();
    semantic::index(&p).unwrap();
    let db = rusqlite::Connection::open(p.data.join("semantic-index/glimpse.sqlite")).unwrap();
    let status = |db: &rusqlite::Connection| -> (String, String) {
        db.query_row("SELECT status,error FROM documents", [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap()
    };
    assert_eq!(status(&db).0, "error");
    db.execute("UPDATE documents SET error='marker'", []).unwrap();
    semantic::index(&p).unwrap();
    assert_eq!(status(&db), ("error".into(), "marker".into()), "unchanged file was retried");
    // A changed file gets another attempt.
    std::thread::sleep(Duration::from_millis(1100));
    fs::write(&broken, "still not a zip").unwrap();
    semantic::index(&p).unwrap();
    assert_ne!(status(&db).1, "marker");
}
#[test]
fn broken_plugin_is_skipped_and_reported() {
    let (_t, p, _c) = fixture();
    let broken = p.data.join("plugins/broken");
    fs::create_dir_all(&broken).unwrap();
    fs::write(broken.join("plugin.toml"), "not = [valid").unwrap();
    plugins::install_builtin(&p).unwrap();
    let listed = plugins::list(&p).unwrap();
    assert_eq!(listed.len(), 1);
    assert!(plugins::last_error(&p, "broken").unwrap().contains("invalid plugin.toml"));
}
#[test]
fn fts_input_is_literal_and_wal_readable() {
    let (_t, p, c) = fixture();
    fs::write(c.roots[0].join("report.txt"), "x").unwrap();
    index::scan(&p, &c).unwrap();
    for q in ["report OR secrets", "\" OR *", "report'", "-", "*", "report NEAR(x)"] {
        assert!(index::search(&p, &c, q, 20).is_ok(), "{q}");
    }
    let a = index::open(&p).unwrap();
    let b = index::open(&p).unwrap();
    a.execute_batch("BEGIN IMMEDIATE").unwrap();
    assert_eq!(index::search(&p, &c, "report", 20).unwrap().len(), 1);
    b.query_row("SELECT count(*) FROM files", [], |r| r.get::<_, i64>(0)).unwrap();
    a.execute_batch("ROLLBACK").unwrap();
    for name in ["filenames.sqlite", "filenames.sqlite-wal", "filenames.sqlite-shm"] {
        assert_eq!(fs::metadata(p.data.join(name)).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
#[test]
fn integration_preserves_edits_and_rejects_tampering() {
    let old = integration::block(true);
    let text = format!("before\n{old}after\n");
    assert_eq!(integration::edit(&text, Some(&old), None).unwrap(), "before\nafter\n");
    assert_eq!(integration::edit(&text, Some(&old), Some(&old)).unwrap(), text);
    assert!(integration::edit(&text.replace("Glimpse\",", "Edited\","), Some(&old), None).is_err());
    assert!(integration::edit("-- BEGIN NIXE GLIMPSE\n", Some(&old), None).is_err());
    let (_t, p, _c) = fixture();
    let user = p.state.join("user.lua");
    fs::write(&user, "original\n").unwrap();
    integration::apply_at(&p, &user, Some(true), false).unwrap();
    integration::apply_at(&p, &user, Some(true), false).unwrap();
    let mut s = fs::read_to_string(&user).unwrap();
    s.push_str("later\n");
    fs::write(&user, s).unwrap();
    integration::apply_at(&p, &user, None, false).unwrap();
    assert!(fs::read_to_string(user).unwrap().contains("original\nlater"));
}
#[test]
fn settings_pages_parse_in_both_real_hosts() {
    let (_t, p, _) = fixture();
    let value = nixe_glimpse::settings::describe(&p, "glimpse").unwrap();
    let titles: Vec<_> = value["groups"].as_array().unwrap().iter().map(|g| g["title"].as_str().unwrap()).collect();
    assert_eq!(titles, ["General", "Shortcuts", "Sources", "Local AI", "Plugins", "Appearance"]);
    // Only long groups collapse; short ones stay open.
    for group in value["groups"].as_array().unwrap() {
        let long = group["rows"].as_array().unwrap().len() > 3;
        assert_eq!(group["collapsible"] == true, long, "{}", group["title"]);
    }
    for group in value["groups"].as_array().unwrap() {
        for row in group["rows"].as_array().unwrap() {
            if row["kind"] == "info" {
                assert!(!row["value"].as_str().unwrap_or("").is_empty(), "Info rows must have visible values");
            }
        }
    }
    let _: current_host::PageDesc = serde_json::from_value(value).unwrap();
}
#[test]
fn subprocess_limits_and_failures() {
    let mut c = Command::new("python3");
    c.args(["-c", "import time; time.sleep(2)"]);
    assert!(process::capture(c, vec![], Duration::from_millis(30), 10).is_err());
    let mut c = Command::new("python3");
    c.args(["-c", "print('a'*10000)"]);
    assert!(process::capture(c, vec![], Duration::from_secs(1), 100).is_err());
    let mut c = Command::new("python3");
    c.args(["-c", "print('ok')"]);
    assert_eq!(process::capture(c, vec![], Duration::from_secs(1), 100).unwrap(), b"ok\n");
}
#[test]
fn plugins_install_disabled_and_execute_only_after_grant() {
    let (_t, p, _) = fixture();
    let id = plugins::install_builtin(&p).unwrap();
    assert!(!plugins::get(&p, &id).unwrap().grant.enabled);
    assert!(plugins::call(&p, &id, "search", "search", json!({"query":"uuid"})).is_err());
    assert!(plugins::enable(&p, &id, false).is_err());
    plugins::enable(&p, &id, true).unwrap();
    let hits = plugins::search(&p, "uuid");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].1.title.len(), 36);
    plugins::disable(&p, &id).unwrap();
    assert!(plugins::search(&p, "uuid").is_empty());
}
#[test]
fn plugin_update_permission_change_disables_and_rolls_back() {
    let (t, p, _) = fixture();
    plugins::install_builtin(&p).unwrap();
    plugins::enable(&p, "quick-tools", true).unwrap();
    let source = t.path().join("updated");
    fs::create_dir(&source).unwrap();
    let old = plugins::get(&p, "quick-tools").unwrap();
    for name in ["plugin.toml", "main.py"] {
        fs::copy(old.dir.join(name), source.join(name)).unwrap();
    }
    let text =
        fs::read_to_string(source.join("plugin.toml")).unwrap().replace("1.0.0", "2.0.0") + "\n[permissions]\nnetwork = true\n";
    fs::write(source.join("plugin.toml"), text).unwrap();
    plugins::install_local(&p, &source, "local", "", true).unwrap();
    assert!(!plugins::get(&p, "quick-tools").unwrap().grant.enabled);
    plugins::rollback(&p, "quick-tools").unwrap();
    assert_eq!(plugins::get(&p, "quick-tools").unwrap().manifest.version, "1.0.0");
    assert!(!plugins::get(&p, "quick-tools").unwrap().grant.enabled);
}
#[test]
fn plugin_package_traversal_and_symlinks_rejected() {
    let (t, p, _) = fixture();
    let source = t.path().join("bad");
    fs::create_dir(&source).unwrap();
    fs::write(
        source.join("plugin.toml"),
        "id='bad'\nname='Bad'\nversion='1'\napi_version=1\nexec=['../escape']\ncapabilities=['search']\n",
    )
    .unwrap();
    assert!(plugins::install_local(&p, &source, "local", "", false).is_err());
    fs::write(
        source.join("plugin.toml"),
        "id='bad'\nname='Bad'\nversion='1'\napi_version=1\nexec=['python3','main.py']\ncapabilities=['search']\n",
    )
    .unwrap();
    symlink("/etc/passwd", source.join("main.py")).unwrap();
    assert!(plugins::install_local(&p, &source, "local", "", false).is_err());
}
#[test]
fn sandbox_blocks_home_and_network() {
    let (t, p, _) = fixture();
    let dir = t.path().join("sandbox");
    fs::create_dir(&dir).unwrap();
    fs::write(
        dir.join("plugin.toml"),
        "id='sandbox'\nname='Sandbox'\nversion='1'\napi_version=1\nexec=['python3','main.py']\ncapabilities=['search']\n",
    )
    .unwrap();
    fs::write(
        dir.join("main.py"),
        r#"import sys,json,socket,os
for line in sys.stdin:
 r=json.loads(line)
 if r['op']=='hello':print(json.dumps({'id':0,'api_version':1}),flush=True)
 elif r['op']=='search':
  assert not os.path.exists('/etc/passwd')
  s=socket.socket()
  try:s.connect(('1.1.1.1',53));blocked=False
  except OSError:blocked=True
  assert blocked
  print(json.dumps({'id':1,'result':[]}),flush=True)
 elif r['op']=='shutdown':break
"#,
    )
    .unwrap();
    plugins::install_local(&p, &dir, "local", "", false).unwrap();
    plugins::enable(&p, "sandbox", true).unwrap();
    plugins::call(&p, "sandbox", "search", "search", json!({"query":"test"})).unwrap();
}
#[test]
fn corrupt_models_are_rejected() {
    let (t, p, _) = fixture();
    let file = t.path().join("model");
    fs::write(&file, b"bad").unwrap();
    assert!(semantic::verify(&file, 3, &"0".repeat(64)).is_err());
    assert_eq!(semantic::status(&p)["runtime"], false);
    assert!(semantic::search(&p, "fixture").unwrap().is_empty());
}

#[test]
fn custom_reader_and_source_revocation() {
    let (_temp, p, c) = fixture();
    let source = c.roots[0].join("ideas.note");
    fs::write(&source, "A nebula contains interstellar dust and gas.").unwrap();
    let example = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/notes-reader");
    plugins::install_local(&p, &example, "local", "", false).unwrap();
    plugins::enable(&p, "notes-reader", true).unwrap();
    index::scan(&p, &c).unwrap();
    index::refresh_readers(&p, &c).unwrap();
    let hits = index::content_search(&p, &c, "nebula").unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].0.path, source);
    plugins::disable(&p, "notes-reader").unwrap();
    assert!(index::content_search(&p, &c, "nebula").unwrap().is_empty());
}
#[test]
fn invalid_vectors_and_shortcut_injection_are_rejected() {
    assert!(semantic::validate_vector(&[0.; 768]).is_err());
    assert!(semantic::validate_vector(&[f32::NAN; 768]).is_err());
    assert!(config::validate_key("SUPER + SPACE\"); os.execute('x')").is_err());
    assert!(config::validate_key("SUPER + SHIFT + G").is_ok());
}

#[test]
fn settings_edits_persist_and_reject_invalid_values() {
    let (_t, p, _) = fixture();
    let set = |key: &str, value: &str| {
        nixe_glimpse::settings::dispatch(&p, &["set".into(), "glimpse".into(), key.into(), value.into()])
    };
    set("search_key", "SUPER + S").unwrap();
    set("menu_key", "SUPER + M").unwrap();
    set("device", "cpu").unwrap();
    set("thermal_limit", "80").unwrap();
    set("rates_date", "2026-10-13").unwrap();
    set("rates", "USD=1, EUR=0.92").unwrap();
    let c = p.load().unwrap();
    assert_eq!(c.search_key, "SUPER + S");
    assert_eq!(c.menu_key, "SUPER + M");
    assert_eq!(c.device, "cpu");
    assert_eq!(c.thermal_limit, 80);
    assert_eq!(c.rates["EUR"], 0.92);
    assert!(set("device", "bad-device").is_err());
    assert!(set("thermal_limit", "200").is_err());
    assert!(set("rates", "USD=0").is_err());
    assert!(set("rates_date", "2026-02-30").is_err());
    assert_eq!(p.load().unwrap().rates, c.rates);
    let exclusion = c.roots[0].join("private");
    set("exclude", exclusion.to_str().unwrap()).unwrap();
    assert_eq!(p.load().unwrap().excluded, vec![exclusion]);
    set("remove-exclude/0", "").unwrap();
    assert!(p.load().unwrap().excluded.is_empty());
}
#[test]
fn corrupt_cached_catalog_uses_bundled_entries() {
    let (_t, p, _) = fixture();
    fs::write(p.data.join("plugin-catalog.json"), b"invalid").unwrap();
    assert!(plugins::catalog(&p, false).unwrap().iter().any(|e| e.id == "quick-tools"));
}

#[test]
fn query_modes_and_related_conversions() {
    use utility::Intent;
    let c = Config::default();
    for q in ["2+2", "2+2=", "2=", "2+", "(2", "=2", "2×3"] {
        assert_eq!(utility::intent(q), Intent::Calculation);
    }
    for q in ["2", "report-2026", "1Password", "Visual Studio"] {
        assert_eq!(utility::intent(q), Intent::Search);
    }
    assert_eq!(utility::answers("2+2", &c).unwrap()[0].title, "2 + 2 = 4");
    assert_eq!(utility::answers("2+2=", &c).unwrap()[0].title, "2 + 2 = 4");
    assert!(utility::answers("2+2==", &c).is_err());
    assert_eq!(utility::answers("2+3*4", &c).unwrap()[0].copy, "14");
    assert!(utility::answers("2+", &c).is_err());
    assert!(utility::answers("2/0", &c).is_err());
    let answers = utility::answers("1oz to pounds", &c).unwrap();
    assert_eq!(answers[0].title, "1 oz = 0.0625 lb");
    assert!(answers.iter().any(|a| a.copy.ends_with(" kg")));
    assert!(answers.iter().any(|a| a.copy.ends_with(" g")));
    assert_eq!(utility::intent("1 oz to"), Intent::Conversion);
    assert!(utility::answers("1 oz to", &c).is_err());
    assert!(utility::answers("1 kg to km", &c).is_err());
    assert_eq!(utility::answers("32 f to c", &c).unwrap()[0].copy, "0 c");
    assert!(utility::answers("1 fl oz to ml", &c).unwrap()[0].copy.starts_with("29.573"));
    assert_eq!(utility::number(1e-15), "1e-15");
}
#[test]
fn lexical_queries_require_all_terms_and_exact_numbers() {
    use nixe_glimpse::query;
    let (_t, p, c) = fixture();
    for name in ["report 2.txt", "report 2026.txt", "report final.txt", "draft final.txt", "report final notes.txt"] {
        fs::write(c.roots[0].join(name), "x").unwrap();
    }
    index::scan(&p, &c).unwrap();
    let hits = index::search(&p, &c, "2", 20).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].name, "report 2.txt");
    let hits = index::search(&p, &c, "report final", 20).unwrap();
    assert_eq!(hits.len(), 2);
    assert!(hits.iter().all(|h| h.name.starts_with("report final")));
    assert_eq!(index::search(&p, &c, "\"report final\"", 20).unwrap().len(), 2);
    assert_eq!(query::name_rank("2 Thing", "2"), Some(1));
    assert_eq!(query::name_rank("2026 Thing", "2"), None);
    assert_eq!(query::name_rank("Visual Studio Code", "visual code"), Some(2));
    assert_eq!(query::name_rank("Visual Studio", "visual code"), None);
    assert_eq!(query::name_rank("Report draft", "\"port draft\""), None);
    assert!(!query::semantic_allowed("2"));
    assert!(!query::semantic_allowed("browser"));
    assert!(query::semantic_allowed("coastal conservation"));
    assert!(query::semantic_allowed("semantic: quokka"));
}
