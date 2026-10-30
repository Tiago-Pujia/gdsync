//! Local state: mirror of the Drive tree (metadata only) + cache bookkeeping.
use crate::drive::{RemoteFile, FOLDER_MIME, GOOGLE_APPS_PREFIX, SHORTCUT_MIME};
use crate::now;
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension, Row};
use std::path::Path;
use std::sync::Mutex;

pub const ROOT_INO: u64 = 1;

#[derive(Clone, Debug)]
pub struct Item {
    pub ino: u64,
    pub id: String,
    pub parent_id: Option<String>,
    pub name: String,
    pub mime: String,
    pub size: u64,
    pub md5: Option<String>,
    pub mtime: i64,
    pub web_link: Option<String>,
    pub pinned: bool,
}

impl Item {
    pub fn is_dir(&self) -> bool {
        self.mime == FOLDER_MIME
    }
    pub fn is_shortcut(&self) -> bool {
        self.mime == SHORTCUT_MIME
    }
    /// Docs/Sheets/Slides/...: no binary content, shown as a link.
    pub fn is_google_doc(&self) -> bool {
        self.mime.starts_with(GOOGLE_APPS_PREFIX) && !self.is_dir() && !self.is_shortcut()
    }
    /// Identifies the remote content version, to know if the cached copy is stale.
    pub fn version(&self) -> String {
        self.md5.clone().unwrap_or_else(|| format!("mtime:{}", self.mtime))
    }
}

const COLS: &str = "ino,id,parent_id,name,mime,size,md5,mtime,web_link,pinned";

fn row(r: &Row) -> rusqlite::Result<Item> {
    Ok(Item {
        ino: r.get::<_, i64>(0)? as u64,
        id: r.get(1)?,
        parent_id: r.get(2)?,
        name: r.get(3)?,
        mime: r.get(4)?,
        size: r.get::<_, i64>(5)? as u64,
        md5: r.get(6)?,
        mtime: r.get(7)?,
        web_link: r.get(8)?,
        pinned: r.get::<_, i64>(9)? != 0,
    })
}

pub struct Db {
    conn: Mutex<Connection>,
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS items(
                 ino INTEGER PRIMARY KEY AUTOINCREMENT,
                 id TEXT NOT NULL UNIQUE,
                 parent_id TEXT,
                 name TEXT NOT NULL,
                 mime TEXT NOT NULL,
                 size INTEGER NOT NULL DEFAULT 0,
                 md5 TEXT,
                 mtime INTEGER NOT NULL DEFAULT 0,
                 web_link TEXT,
                 pinned INTEGER NOT NULL DEFAULT 0
             );
             CREATE INDEX IF NOT EXISTS items_parent ON items(parent_id);
             CREATE TABLE IF NOT EXISTS kv(key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS cache(
                 id TEXT PRIMARY KEY,
                 version TEXT NOT NULL,
                 size INTEGER NOT NULL,
                 last_access INTEGER NOT NULL
             );",
        )?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ---- items ----

    fn upsert_with(conn: &Connection, f: &RemoteFile, ino: Option<u64>) -> rusqlite::Result<()> {
        let size: i64 = f.size.as_deref().and_then(|s| s.parse().ok()).unwrap_or(0);
        let mtime = f.modified_time.as_deref().and_then(parse_rfc3339).unwrap_or(0);
        // Root is the only item without parent; Drive is single-parent since 2020.
        let parent = if ino == Some(ROOT_INO) { None } else { f.parents.first().cloned() };
        conn.execute(
            "INSERT INTO items(ino,id,parent_id,name,mime,size,md5,mtime,web_link)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)
             ON CONFLICT(id) DO UPDATE SET
                 parent_id=excluded.parent_id, name=excluded.name, mime=excluded.mime,
                 size=excluded.size, md5=excluded.md5, mtime=excluded.mtime,
                 web_link=excluded.web_link",
            params![ino.map(|i| i as i64), f.id, parent, f.name, f.mime_type, size, f.md5_checksum, mtime, f.web_view_link],
        )?;
        Ok(())
    }

    pub fn upsert_root(&self, f: &RemoteFile) -> Result<()> {
        Self::upsert_with(&self.conn(), f, Some(ROOT_INO))?;
        self.kv_set("root_id", &f.id)
    }

    pub fn upsert(&self, f: &RemoteFile) -> Result<()> {
        let root = self.kv_get("root_id")?;
        let ino = (root.as_deref() == Some(f.id.as_str())).then_some(ROOT_INO);
        Self::upsert_with(&self.conn(), f, ino)?;
        Ok(())
    }

    pub fn upsert_many(&self, files: &[RemoteFile]) -> Result<()> {
        let root = self.kv_get("root_id")?;
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        for f in files {
            let ino = (root.as_deref() == Some(f.id.as_str())).then_some(ROOT_INO);
            Self::upsert_with(&tx, f, ino)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn remove(&self, id: &str) -> Result<()> {
        self.conn().execute("DELETE FROM items WHERE id=?1", [id])?;
        Ok(())
    }

    pub fn by_ino(&self, ino: u64) -> Result<Option<Item>> {
        let sql = format!("SELECT {COLS} FROM items WHERE ino=?1");
        Ok(self.conn().query_row(&sql, [ino as i64], row).optional()?)
    }

    pub fn by_id(&self, id: &str) -> Result<Option<Item>> {
        let sql = format!("SELECT {COLS} FROM items WHERE id=?1");
        Ok(self.conn().query_row(&sql, [id], row).optional()?)
    }

    /// Children ordered by (name, id) so duplicate names get stable suffixes.
    pub fn children(&self, parent_id: &str) -> Result<Vec<Item>> {
        let sql = format!("SELECT {COLS} FROM items WHERE parent_id=?1 ORDER BY name, id");
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(&sql)?;
        let items = stmt.query_map([parent_id], row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(items)
    }

    pub fn count_items(&self) -> Result<u64> {
        Ok(self.conn().query_row("SELECT COUNT(*) FROM items", [], |r| r.get::<_, i64>(0))? as u64)
    }

    // ---- key/value ----

    pub fn kv_get(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn()
            .query_row("SELECT value FROM kv WHERE key=?1", [key], |r| r.get(0))
            .optional()?)
    }

    pub fn kv_set(&self, key: &str, value: &str) -> Result<()> {
        self.conn().execute(
            "INSERT INTO kv(key,value) VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [key, value],
        )?;
        Ok(())
    }

    // ---- cache bookkeeping ----

    pub fn cache_version(&self, id: &str) -> Result<Option<String>> {
        Ok(self
            .conn()
            .query_row("SELECT version FROM cache WHERE id=?1", [id], |r| r.get(0))
            .optional()?)
    }

    pub fn cache_put(&self, id: &str, version: &str, size: u64) -> Result<()> {
        self.conn().execute(
            "INSERT INTO cache(id,version,size,last_access) VALUES(?1,?2,?3,?4)
             ON CONFLICT(id) DO UPDATE SET version=excluded.version, size=excluded.size, last_access=excluded.last_access",
            params![id, version, size as i64, now()],
        )?;
        Ok(())
    }

    pub fn cache_touch(&self, id: &str) -> Result<()> {
        self.conn().execute("UPDATE cache SET last_access=?2 WHERE id=?1", params![id, now()])?;
        Ok(())
    }

    pub fn cache_remove(&self, id: &str) -> Result<()> {
        self.conn().execute("DELETE FROM cache WHERE id=?1", [id])?;
        Ok(())
    }

    pub fn cache_total(&self) -> Result<u64> {
        Ok(self
            .conn()
            .query_row("SELECT COALESCE(SUM(size),0) FROM cache", [], |r| r.get::<_, i64>(0))? as u64)
    }

    /// Eviction candidates: orphans first, then least recently used; never pinned files.
    pub fn cache_evictable(&self) -> Result<Vec<(String, u64)>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT c.id, c.size FROM cache c LEFT JOIN items i ON i.id=c.id
             WHERE COALESCE(i.pinned,0)=0
             ORDER BY (i.id IS NOT NULL), c.last_access",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

/// Parses the UTC timestamps Drive returns ("2024-05-01T12:34:56.789Z") into Unix seconds.
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (n(0..4)?, n(5..7)?, n(8..10)?);
    let (hh, mm, ss) = (n(11..13)?, n(14..16)?, n(17..19)?);
    // Days from civil date (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + hh * 3600 + mm * 60 + ss)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rf(id: &str, name: &str, parent: &str, mime: &str) -> RemoteFile {
        RemoteFile {
            id: id.into(),
            name: name.into(),
            mime_type: mime.into(),
            size: Some("42".into()),
            md5_checksum: Some("abc".into()),
            modified_time: Some("2024-05-01T12:34:56.789Z".into()),
            parents: vec![parent.into()],
            trashed: false,
            web_view_link: None,
        }
    }

    #[test]
    fn rfc3339() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("2024-05-01T12:34:56.789Z"), Some(1714566896));
        assert_eq!(parse_rfc3339("2000-02-29T00:00:00Z"), Some(951782400));
        assert_eq!(parse_rfc3339("garbage"), None);
    }

    #[test]
    fn root_is_ino_1_and_inodes_are_stable() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_root(&rf("ROOT", "Mi unidad", "", FOLDER_MIME)).unwrap();
        db.upsert_many(&[rf("a", "a.txt", "ROOT", "text/plain"), rf("b", "Docs", "ROOT", FOLDER_MIME)])
            .unwrap();
        let root = db.by_ino(ROOT_INO).unwrap().unwrap();
        assert_eq!(root.id, "ROOT");
        assert_eq!(root.parent_id, None);

        let a = db.by_id("a").unwrap().unwrap();
        assert_eq!(a.size, 42);
        // Renaming keeps the inode.
        db.upsert(&rf("a", "renamed.txt", "ROOT", "text/plain")).unwrap();
        let a2 = db.by_id("a").unwrap().unwrap();
        assert_eq!((a2.ino, a2.name.as_str()), (a.ino, "renamed.txt"));

        assert_eq!(db.children("ROOT").unwrap().len(), 2);
        db.remove("a").unwrap();
        assert_eq!(db.children("ROOT").unwrap().len(), 1);
    }

    #[test]
    fn eviction_skips_pinned_and_prefers_orphans() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_root(&rf("ROOT", "Mi unidad", "", FOLDER_MIME)).unwrap();
        db.upsert_many(&[rf("a", "a", "ROOT", "x/y"), rf("p", "p", "ROOT", "x/y")]).unwrap();
        db.conn().execute("UPDATE items SET pinned=1 WHERE id='p'", []).unwrap();
        db.cache_put("a", "v", 10).unwrap();
        db.cache_put("p", "v", 10).unwrap();
        db.cache_put("orphan", "v", 10).unwrap();
        let ids: Vec<_> = db.cache_evictable().unwrap().into_iter().map(|(id, _)| id).collect();
        assert_eq!(ids, vec!["orphan", "a"]);
        assert_eq!(db.cache_total().unwrap(), 30);
    }
}
