use crate::config::{self, Config, Paths};
use anyhow::Result;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FileHit {
    pub path: PathBuf,
    pub name: String,
}
pub fn open(paths: &Paths) -> Result<Connection> {
    paths.ensure()?;
    let db = paths.data.join("filenames.sqlite");
    if db.symlink_metadata().is_ok_and(|m| m.file_type().is_symlink()) {
        anyhow::bail!("index is a symlink");
    }
    let conn = Connection::open(&db)?;
    conn.busy_timeout(std::time::Duration::from_secs(3))?;
    conn.execute_batch("PRAGMA journal_mode=WAL; CREATE TABLE IF NOT EXISTS files(path TEXT PRIMARY KEY,name TEXT NOT NULL,root TEXT NOT NULL,mtime INTEGER,size INTEGER); CREATE VIRTUAL TABLE IF NOT EXISTS names USING fts5(path UNINDEXED,name,tokenize='unicode61'); CREATE TRIGGER IF NOT EXISTS file_add AFTER INSERT ON files BEGIN INSERT INTO names(path,name) VALUES(new.path,new.name); END; CREATE TRIGGER IF NOT EXISTS file_del AFTER DELETE ON files BEGIN DELETE FROM names WHERE path=old.path; END; CREATE TRIGGER IF NOT EXISTS file_update AFTER UPDATE ON files BEGIN DELETE FROM names WHERE path=old.path; INSERT INTO names(path,name) VALUES(new.path,new.name); END;")?;
    use std::os::unix::fs::PermissionsExt;
    for name in ["filenames.sqlite", "filenames.sqlite-wal", "filenames.sqlite-shm"] {
        let p = paths.data.join(name);
        if p.exists() {
            fs::set_permissions(p, fs::Permissions::from_mode(0o600))?;
        }
    }
    conn.execute_batch("CREATE VIRTUAL TABLE IF NOT EXISTS extracted USING fts5(path UNINDEXED,plugin UNINDEXED,version UNINDEXED,stamp UNINDEXED,info UNINDEXED,text,tokenize='unicode61');")?;
    Ok(conn)
}
fn walk(c: &Config, p: &Path, found: &mut Vec<(PathBuf, u64, u64)>) -> Result<()> {
    if !config::allowed(c, p) {
        return Ok(());
    }
    let meta = fs::symlink_metadata(p)?;
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    if meta.is_dir() {
        for entry in fs::read_dir(p)? {
            walk(c, &entry?.path(), found)?;
        }
    } else if meta.is_file() {
        found.push((p.to_owned(), meta.modified()?.duration_since(UNIX_EPOCH)?.as_secs(), meta.len()));
    }
    Ok(())
}
pub fn scan(paths: &Paths, c: &Config) -> Result<serde_json::Value> {
    let mut conn = open(paths)?;
    let mut count = 0;
    let mut unavailable = Vec::new();
    for root in &c.roots {
        let mut found = Vec::new();
        if let Err(e) = walk(c, root, &mut found) {
            unavailable.push(format!("{}: {e}", root.display()));
            continue;
        }
        // A whole root is committed only after traversal succeeded; interrupted/unreadable roots are never pruned.
        let latest = paths.load()?;
        if latest.paused || latest.roots != c.roots || latest.excluded != c.excluded {
            break;
        }
        let tx = conn.transaction()?;
        tx.execute_batch("CREATE TEMP TABLE IF NOT EXISTS seen(path TEXT PRIMARY KEY); DELETE FROM seen;")?;
        for (path, mtime, size) in &found {
            let Some(s) = path.to_str() else {
                continue;
            };
            tx.execute("INSERT INTO seen(path) VALUES(?1)", [s])?;
            tx.execute("INSERT INTO files(path,name,root,mtime,size) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(path) DO UPDATE SET name=excluded.name,root=excluded.root,mtime=excluded.mtime,size=excluded.size WHERE files.mtime!=excluded.mtime OR files.size!=excluded.size",params![s,path.file_name().unwrap().to_string_lossy(),root.to_string_lossy(),*mtime as i64,*size as i64])?;
        }
        tx.execute("DELETE FROM files WHERE root=?1 AND path NOT IN (SELECT path FROM seen)", [root.to_string_lossy().as_ref()])?;
        tx.commit()?;
        count += found.len();
    }
    let latest = paths.load()?;
    purge_outside(&conn, &latest)?;
    Ok(serde_json::json!({"files_seen":count,"unavailable":unavailable}))
}
pub fn purge_outside(conn: &Connection, c: &Config) -> Result<()> {
    let paths: Vec<String> =
        conn.prepare("SELECT path FROM files")?.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    for p in paths {
        if !config::allowed(c, Path::new(&p)) {
            conn.execute("DELETE FROM files WHERE path=?1", [p])?;
        }
    }
    conn.execute("DELETE FROM extracted WHERE path NOT IN (SELECT path FROM files)", [])?;
    Ok(())
}
pub fn search(paths: &Paths, c: &Config, q: &str, limit: usize) -> Result<Vec<FileHit>> {
    if q.trim().is_empty() {
        return Ok(vec![]);
    }
    let conn = open(paths)?;
    let expr = crate::query::fts(q);
    if expr.is_empty() {
        return Ok(vec![]);
    }
    let mut stmt = conn.prepare("SELECT path,name FROM names WHERE names MATCH ?1 ORDER BY CASE WHEN lower(name)=lower(?3) THEN 0 WHEN instr(lower(name),lower(?3))=1 THEN 1 ELSE 2 END, bm25(names) LIMIT ?2")?;
    let candidates = stmt.query_map(params![expr, (limit * 20).min(2000) as i64, q.trim().trim_matches('"')], |r| {
        Ok(FileHit { path: PathBuf::from(r.get::<_, String>(0)?), name: r.get(1)? })
    })?;
    let mut hits = Vec::new();
    for hit in candidates {
        let hit = hit?;
        if config::allowed(c, &hit.path) {
            hits.push(hit);
            if hits.len() == limit {
                break;
            }
        }
    }
    Ok(hits)
}
pub fn purge(paths: &Paths) -> Result<()> {
    // Caller must stop daemon/helpers first.
    for name in ["filenames.sqlite", "filenames.sqlite-wal", "filenames.sqlite-shm"] {
        match fs::remove_file(paths.data.join(name)) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    let semantic = paths.data.join("semantic-index");
    if semantic.exists() {
        fs::remove_dir_all(semantic)?;
    }
    Ok(())
}

/// Readers contribute bounded, versioned segments; disabled/revoked readers disappear immediately.
pub fn refresh_readers(paths: &Paths, _c: &Config) -> Result<()> {
    let conn = open(paths)?;
    let plugins = crate::plugins::list(paths)?;
    let files: Vec<(String, i64, i64)> = conn
        .prepare("SELECT path,mtime,size FROM files")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<_>>()?;
    for plugin in plugins.into_iter().filter(|p| p.grant.enabled && p.manifest.capabilities.contains(&"reader".into())) {
        for (path, mtime, size) in &files {
            let current = paths.load()?;
            if current.paused {
                return Ok(());
            }
            let pathbuf = Path::new(path);
            if !config::allowed(&current, pathbuf) || !plugin.grant.permissions.read.iter().any(|p| pathbuf.starts_with(p)) {
                continue;
            }
            let ext = pathbuf.extension().unwrap_or_default().to_string_lossy().to_lowercase();
            if !plugin.manifest.file_types.contains(&ext) {
                continue;
            }
            let stamp = format!("{mtime}:{size}");
            let cached: bool = conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM extracted WHERE path=?1 AND plugin=?2 AND version=?3 AND stamp=?4)",
                params![path, plugin.manifest.id, plugin.manifest.version, stamp],
                |r| r.get(0),
            )?;
            if cached {
                continue;
            }
            let value =
                match crate::plugins::call(paths, &plugin.manifest.id, "reader", "extract", serde_json::json!({"path":path})) {
                    Ok(v) => v,
                    Err(e) => {
                        crate::plugins::record_error(paths, &plugin.manifest.id, &e.to_string())?;
                        continue;
                    }
                };
            let segments: Vec<crate::sdk::Segment> = serde_json::from_value(value)?;
            crate::sdk::validate_segments(&segments)?;
            conn.execute("DELETE FROM extracted WHERE path=?1 AND plugin=?2", params![path, plugin.manifest.id])?;
            for segment in segments {
                conn.execute(
                    "INSERT INTO extracted(path,plugin,version,stamp,info,text) VALUES(?1,?2,?3,?4,?5,?6)",
                    params![
                        path,
                        plugin.manifest.id,
                        plugin.manifest.version,
                        stamp,
                        serde_json::to_string(&segment)?,
                        segment.text
                    ],
                )?;
            }
        }
    }
    conn.execute("DELETE FROM extracted WHERE path NOT IN (SELECT path FROM files)", [])?;
    Ok(())
}
pub fn content_search(paths: &Paths, c: &Config, q: &str) -> Result<Vec<(FileHit, serde_json::Value)>> {
    if q.trim().is_empty() {
        return Ok(vec![]);
    }
    let conn = open(paths)?;
    let expr = crate::query::fts(q);
    if expr.is_empty() {
        return Ok(vec![]);
    }
    let mut stmt = conn
        .prepare("SELECT path,plugin,version,info FROM extracted WHERE extracted MATCH ?1 ORDER BY bm25(extracted) LIMIT 100")?;
    let plugins = crate::plugins::list(paths)?;
    let mut result = Vec::new();
    for item in stmt.query_map([expr], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?))
    })? {
        let (path, id, version, info) = item?;
        let path = PathBuf::from(path);
        if !config::allowed(c, &path)
            || !plugins.iter().any(|p| {
                p.manifest.id == id
                    && p.grant.enabled
                    && p.manifest.version == version
                    && p.grant.permissions.read.iter().any(|r| path.starts_with(r))
            })
        {
            continue;
        }
        result.push((
            FileHit { name: path.file_name().unwrap_or_default().to_string_lossy().into_owned(), path },
            serde_json::json!({"info":serde_json::from_str::<serde_json::Value>(&info)?,"reader":id}),
        ));
        if result.len() == 20 {
            break;
        }
    }
    Ok(result)
}
