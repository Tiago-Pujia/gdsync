//! Local state: mirror of the Drive tree (metadata only) + cache bookkeeping.
use crate::drive::{RemoteFile, FOLDER_MIME, GOOGLE_APPS_PREFIX, SHORTCUT_MIME};
use crate::now;
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension, Row};
use std::collections::{HashMap, HashSet};
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
    /// Docs/Sheets/Slides/... and shortcuts: no binary content, shown as a link.
    pub fn is_google_doc(&self) -> bool {
        self.mime.starts_with(GOOGLE_APPS_PREFIX) && !self.is_dir()
    }
    /// Identifies the remote content version, to know if the cached copy is stale.
    pub fn version(&self) -> String {
        self.md5.clone().unwrap_or_else(|| format!("mtime:{}", self.mtime))
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct UploadTask {
    pub id: i64,
    pub file_id: String,
    pub op: String,
    pub payload: Option<String>,
    pub status: String,
    pub created_at: i64,
    pub attempts: i64,
    pub last_error: Option<String>,
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

fn upload_row(r: &Row) -> rusqlite::Result<UploadTask> {
    Ok(UploadTask {
        id: r.get(0)?,
        file_id: r.get(1)?,
        op: r.get(2)?,
        payload: r.get(3)?,
        status: r.get(4)?,
        created_at: r.get(5)?,
        attempts: r.get(6)?,
        last_error: r.get(7)?,
    })
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct QueueSummary {
    pub total: u64,
    pub uploads: u64,
    pub upload_bytes: u64,
    pub trashes: u64,
    pub others: u64,
    pub failed: u64,
}

pub const DOC_EXTENSIONS: &[&str] = &[
    ".gdoc.desktop",
    ".gsheet.desktop",
    ".gslides.desktop",
    ".gdraw.desktop",
    ".gform.desktop",
    ".gjam.desktop",
    ".gmap.desktop",
    ".gsite.desktop",
    ".glink.desktop",
    ".desktop",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinnedRootItem {
    pub id: String,
    pub name: String,
    pub mime: String,
    pub is_folder: bool,
    pub full_path: String,
    pub file_count: u64,
    pub total_size: u64,
    pub cached_count: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalFileStatus {
    Folder,
    Pinned,
    Cached,
    CloudOnly,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExplorerItem {
    pub id: String,
    pub name: String,
    pub rel_path: String,
    pub parent_location: String,
    pub size: u64,
    pub mime: String,
    pub is_folder: bool,
    pub pinned: bool,
    pub is_root_pinned: bool,
    pub status: LocalFileStatus,
    pub web_link: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueItemView {
    pub id: i64,
    pub op: String,
    pub file_id: String,
    pub display_name: String,
    pub size_bytes: u64,
    pub status: String,
    pub attempts: i64,
    pub created_at: i64,
    pub last_error: Option<String>,
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
             );
             CREATE TABLE IF NOT EXISTS upload_queue(
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 file_id TEXT NOT NULL,
                 op TEXT NOT NULL,
                 payload TEXT,
                 status TEXT NOT NULL DEFAULT 'pending',
                 created_at INTEGER NOT NULL,
                 attempts INTEGER NOT NULL DEFAULT 0,
                 last_error TEXT
             );
             CREATE INDEX IF NOT EXISTS upload_queue_status ON upload_queue(status);",
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
        let web_link = if f.mime_type == SHORTCUT_MIME {
            if let Some(details) = &f.shortcut_details {
                if let Some(target_id) = &details.target_id {
                    if details.target_mime_type.as_deref() == Some(FOLDER_MIME) {
                        Some(format!("https://drive.google.com/drive/folders/{target_id}"))
                    } else {
                        Some(format!("https://drive.google.com/open?id={target_id}"))
                    }
                } else {
                    Some(format!("https://drive.google.com/open?id={}", f.id))
                }
            } else {
                Some(format!("https://drive.google.com/open?id={}", f.id))
            }
        } else {
            f.web_view_link.clone()
        };
        conn.execute(
            "INSERT INTO items(ino,id,parent_id,name,mime,size,md5,mtime,web_link)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)
             ON CONFLICT(id) DO UPDATE SET
                 parent_id=excluded.parent_id, name=excluded.name, mime=excluded.mime,
                 size=excluded.size, md5=excluded.md5, mtime=excluded.mtime,
                 web_link=excluded.web_link",
            params![ino.map(|i| i as i64), f.id, parent, f.name, f.mime_type, size, f.md5_checksum, mtime, web_link],
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

    pub fn insert_local_item(&self, id: &str, parent_id: Option<&str>, name: &str, mime: &str, size: u64, mtime: i64) -> Result<u64> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO items(id, parent_id, name, mime, size, mtime)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(id) DO UPDATE SET
                 parent_id=excluded.parent_id, name=excluded.name, mime=excluded.mime,
                 size=excluded.size, mtime=excluded.mtime",
            params![id, parent_id, name, mime, size as i64, mtime],
        )?;
        let ino: i64 = conn.query_row("SELECT ino FROM items WHERE id=?1", [id], |r| r.get(0))?;
        Ok(ino as u64)
    }

    pub fn update_item_size_mtime(&self, id: &str, size: u64, mtime: i64) -> Result<()> {
        self.conn().execute(
            "UPDATE items SET size=?2, mtime=?3 WHERE id=?1",
            params![id, size as i64, mtime],
        )?;
        Ok(())
    }

    pub fn rename_item(&self, id: &str, new_name: &str, new_parent: Option<&str>) -> Result<()> {
        let conn = self.conn();
        if let Some(parent) = new_parent {
            conn.execute(
                "UPDATE items SET name=?2, parent_id=?3 WHERE id=?1",
                params![id, new_name, parent],
            )?;
        } else {
            conn.execute(
                "UPDATE items SET name=?2 WHERE id=?1",
                params![id, new_name],
            )?;
        }
        Ok(())
    }

    pub fn replace_file_id(&self, old_id: &str, new_id: &str, md5: Option<&str>, mtime: i64) -> Result<()> {
        let version = md5.map(|s| s.to_string()).unwrap_or_else(|| format!("mtime:{mtime}"));
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "UPDATE items SET id=?1, md5=?2, mtime=?3 WHERE id=?4",
            params![new_id, md5, mtime, old_id],
        )?;
        tx.execute(
            "UPDATE items SET parent_id=?1 WHERE parent_id=?2",
            params![new_id, old_id],
        )?;
        tx.execute(
            "UPDATE cache SET id=?1, version=?3 WHERE id=?2",
            params![new_id, old_id, version],
        )?;
        tx.execute(
            "UPDATE upload_queue SET file_id=?1 WHERE file_id=?2",
            params![new_id, old_id],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn replace_existing_with_local(
        &self,
        existing_id: &str,
        new_ino: u64,
        new_name: &str,
        parent_id: &str,
        size: u64,
        mtime: i64,
    ) -> Result<()> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let (web_link, pinned, mime): (Option<String>, i64, String) = tx
            .query_row(
                "SELECT web_link, pinned, mime FROM items WHERE id=?1",
                [existing_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap_or((None, 0, "application/octet-stream".to_string()));

        tx.execute("DELETE FROM items WHERE id=?1", [existing_id])?;
        tx.execute(
            "UPDATE items SET id=?1, name=?2, parent_id=?3, size=?4, md5=NULL, mtime=?5, web_link=?7, pinned=?8, mime=?9 WHERE ino=?6",
            params![existing_id, new_name, parent_id, size as i64, mtime, new_ino as i64, web_link, pinned, mime],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn set_pinned(&self, id: &str, pinned: bool) -> Result<()> {
        self.conn().execute(
            "UPDATE items SET pinned=?2 WHERE id=?1",
            params![id, if pinned { 1 } else { 0 }],
        )?;
        Ok(())
    }

    pub fn count_pinned_items(&self) -> Result<u64> {
        Ok(self.conn().query_row(
            "SELECT COUNT(*) FROM items WHERE pinned = 1",
            [],
            |r| r.get::<_, i64>(0),
        )? as u64)
    }

    pub fn get_root_pinned_items(&self) -> Result<Vec<PinnedRootItem>> {
        let conn = self.conn();
        let query = format!(
            "WITH RECURSIVE
            path_builder(id, name, parent_id, mime, size, full_path) AS (
                SELECT id, name, parent_id, mime, size, name as full_path
                FROM items
                WHERE parent_id = (SELECT id FROM items WHERE ino={ROOT_INO})
                UNION ALL
                SELECT i.id, i.name, i.parent_id, i.mime, i.size, pb.full_path || '/' || i.name
                FROM items i JOIN path_builder pb ON i.parent_id = pb.id
            ),
            ancestors(item_id, ancestor_id, ancestor_pinned) AS (
                SELECT i.id, i.parent_id, p.pinned
                FROM items i
                JOIN items p ON i.parent_id = p.id
                UNION ALL
                SELECT a.item_id, p.parent_id, p.pinned
                FROM ancestors a
                JOIN items p ON a.ancestor_id = p.id
                WHERE p.parent_id IS NOT NULL
            )
            SELECT pb.id, pb.name, pb.mime, pb.size, pb.full_path
            FROM path_builder pb
            JOIN items i ON pb.id = i.id
            WHERE i.pinned = 1
              AND NOT EXISTS (
                  SELECT 1 FROM ancestors a
                  WHERE a.item_id = pb.id AND a.ancestor_pinned = 1
              )
            ORDER BY (pb.mime = '{FOLDER_MIME}') DESC, pb.full_path ASC;"
        );

        let mut stmt = conn.prepare(&query)?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)? as u64,
                    r.get::<_, String>(4)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut results = Vec::new();
        for (fid, name, mime, size, full_path) in rows {
            let is_folder = mime == FOLDER_MIME;
            if is_folder {
                let (file_count, total_size, cached_count): (i64, i64, i64) = conn.query_row(
                    &format!(
                        "WITH RECURSIVE sub(id, size, mime) AS (
                            SELECT id, size, mime FROM items WHERE id = ?1
                            UNION ALL
                            SELECT i.id, i.size, i.mime FROM items i JOIN sub s ON i.parent_id = s.id
                        )
                        SELECT 
                            COUNT(CASE WHEN mime != '{FOLDER_MIME}' AND mime NOT LIKE '{GOOGLE_APPS_PREFIX}%' THEN 1 END),
                            COALESCE(SUM(CASE WHEN mime != '{FOLDER_MIME}' AND mime NOT LIKE '{GOOGLE_APPS_PREFIX}%' THEN size END), 0),
                            COUNT(CASE WHEN id IN (SELECT id FROM cache) AND mime != '{FOLDER_MIME}' THEN 1 END)
                        FROM sub;"
                    ),
                    [&fid],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )?;
                results.push(PinnedRootItem {
                    id: fid,
                    name,
                    mime,
                    is_folder: true,
                    full_path,
                    file_count: file_count as u64,
                    total_size: total_size as u64,
                    cached_count: cached_count as u64,
                });
            } else {
                let is_cached: bool = conn
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM cache WHERE id = ?1)",
                        [&fid],
                        |r| r.get(0),
                    )
                    .unwrap_or(false);
                results.push(PinnedRootItem {
                    id: fid,
                    name,
                    mime,
                    is_folder: false,
                    full_path,
                    file_count: 1,
                    total_size: size,
                    cached_count: if is_cached { 1 } else { 0 },
                });
            }
        }
        Ok(results)
    }

    pub fn set_pinned_recursive(&self, id: &str, pinned: bool) -> Result<Vec<Item>> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute(
            "WITH RECURSIVE subfolder(id) AS (
                 SELECT id FROM items WHERE id = ?1
                 UNION ALL
                 SELECT i.id FROM items i JOIN subfolder s ON i.parent_id = s.id
             )
             UPDATE items SET pinned = ?2 WHERE id IN (SELECT id FROM subfolder)",
            params![id, if pinned { 1 } else { 0 }],
        )?;

        let mut download_targets = Vec::new();
        if pinned {
            let sql = format!(
                "WITH RECURSIVE subfolder(id) AS (
                     SELECT id FROM items WHERE id = ?1
                     UNION ALL
                     SELECT i.id FROM items i JOIN subfolder s ON i.parent_id = s.id
                 )
                 SELECT {COLS} FROM items
                 WHERE id IN (SELECT id FROM subfolder)
                   AND mime != '{FOLDER_MIME}'
                   AND mime NOT LIKE '{GOOGLE_APPS_PREFIX}%'
                   AND id NOT IN (SELECT id FROM cache)"
            );
            let mut stmt = tx.prepare(&sql)?;
            let items = stmt.query_map([id], row)?.collect::<rusqlite::Result<Vec<_>>>()?;
            download_targets = items;
        }
        tx.commit()?;
        Ok(download_targets)
    }

    pub fn list_explorer_items(&self, limit: usize) -> Result<Vec<ExplorerItem>> {
        let root_pinned_items = self.get_root_pinned_items()?;
        let root_pinned_ids: HashSet<String> = root_pinned_items.into_iter().map(|it| it.id).collect();

        let conn = self.conn();
        let mut map_stmt = conn.prepare_cached("SELECT id, parent_id, name FROM items")?;
        let parent_map: HashMap<String, (Option<String>, String)> = map_stmt
            .query_map([], |r| Ok((r.get(0)?, (r.get(1)?, r.get(2)?))))?
            .collect::<rusqlite::Result<_>>()?;

        let mut cache_stmt = conn.prepare_cached("SELECT id FROM cache")?;
        let cached_set: HashSet<String> = cache_stmt
            .query_map([], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;

        let resolve_rel_path = |item_id: &str| -> String {
            let mut parts = Vec::new();
            let mut curr = Some(item_id.to_string());
            let mut visited = HashSet::new();
            while let Some(id) = curr {
                if !visited.insert(id.clone()) {
                    break;
                }
                if let Some((pid, name)) = parent_map.get(&id) {
                    if pid.is_none() {
                        break;
                    }
                    parts.push(name.clone());
                    curr = pid.clone();
                } else {
                    break;
                }
            }
            parts.reverse();
            parts.join("/")
        };

        let sql = format!(
            "SELECT id, name, size, mime, pinned, web_link
             FROM items
             WHERE ino != {ROOT_INO}
             ORDER BY (mime = '{FOLDER_MIME}') DESC, name ASC
             LIMIT ?1"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map([limit as i64], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)? as u64,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)? != 0,
                    r.get::<_, Option<String>>(5)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut items = Vec::with_capacity(rows.len());
        for (id, name, size, mime, pinned, web_link) in rows {
            let is_folder = mime == FOLDER_MIME;
            let rel_path = resolve_rel_path(&id);
            let parent_location = if let Some(idx) = rel_path.rfind('/') {
                rel_path[..idx].to_string()
            } else {
                "(Raíz)".to_string()
            };
            let is_root_pinned = root_pinned_ids.contains(&id);
            let status = if is_folder {
                LocalFileStatus::Folder
            } else if pinned {
                LocalFileStatus::Pinned
            } else if cached_set.contains(&id) {
                LocalFileStatus::Cached
            } else {
                LocalFileStatus::CloudOnly
            };
            let link = web_link.unwrap_or_else(|| format!("https://drive.google.com/open?id={id}"));

            items.push(ExplorerItem {
                id,
                name,
                rel_path,
                parent_location,
                size,
                mime,
                is_folder,
                pinned,
                is_root_pinned,
                status,
                web_link: link,
            });
        }

        Ok(items)
    }

    pub fn list_root_folders(&self) -> Result<Vec<(String, String)>> {
        let root_id = self.kv_get("root_id")?.unwrap_or_default();
        let conn = self.conn();
        let sql = format!(
            "SELECT id, name FROM items
             WHERE mime = '{FOLDER_MIME}' AND (
                parent_id = 'root' OR parent_id = '' OR parent_id IS NULL
                OR (?1 != '' AND parent_id = ?1)
             ) AND id != ?1 AND name != 'Mi unidad'
             ORDER BY name COLLATE NOCASE ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![root_id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let mut res = Vec::new();
        for r in rows {
            res.push(r?);
        }
        Ok(res)
    }

    pub fn lookup_child(&self, parent_id: &str, name: &str) -> Result<Option<Item>> {
        let sql = format!("SELECT {COLS} FROM items WHERE parent_id=?1 AND name=?2");
        Ok(self.conn().query_row(&sql, params![parent_id, name], row).optional()?)
    }

    /// Resolves a relative path (e.g. "Docs/notes.txt") to its database Item.
    pub fn resolve_relative_path(&self, rel_path: &Path) -> Result<Option<Item>> {
        let root = match self.by_ino(ROOT_INO)? {
            Some(r) => r,
            None => return Ok(None),
        };
        let mut current_id = root.id.clone();
        let mut current_item = root;

        for comp in rel_path.components() {
            let seg = match comp {
                std::path::Component::Normal(s) => s.to_string_lossy(),
                std::path::Component::CurDir => continue,
                _ => continue,
            };
            if seg.is_empty() {
                continue;
            }
            let children = self.children(&current_id)?;
            let entries = crate::names::local_entries(children);

            // 1. Exact match on local_entries
            if let Some((_, child)) = entries.iter().find(|(name, _)| name == &seg) {
                current_id = child.id.clone();
                current_item = child.clone();
                continue;
            }

            // 2. Strip Google Doc extensions
            let mut matched_child = None;
            for ext in DOC_EXTENSIONS {
                if let Some(stripped) = seg.strip_suffix(ext) {
                    if let Some((_, child)) = entries.iter().find(|(name, _)| name == stripped) {
                        matched_child = Some(child.clone());
                        break;
                    }
                }
            }
            if let Some(child) = matched_child {
                current_id = child.id.clone();
                current_item = child;
                continue;
            }

            // 3. Sanitized slashes (∕ <-> /)
            let slash_converted = seg.replace('\u{2215}', "/");
            if let Some((_, child)) = entries.iter().find(|(name, _)| name == &slash_converted) {
                current_id = child.id.clone();
                current_item = child.clone();
                continue;
            }

            // 4. Fallback: match item.name directly or sanitized name
            if let Some((_, child)) = entries.iter().find(|(_, c)| {
                let s = c.name.replace('/', "\u{2215}");
                s == seg || format!("{s}.desktop") == seg
            }) {
                current_id = child.id.clone();
                current_item = child.clone();
                continue;
            }

            return Ok(None);
        }
        Ok(Some(current_item))
    }

    // ---- upload queue ----

    pub fn enqueue_upload(&self, file_id: &str, op: &str, payload: Option<&str>) -> Result<i64> {
        let conn = self.conn();
        // Coalesce / deduplicate pending upload tasks
        if op == "update" {
            let exists: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM upload_queue WHERE file_id=?1 AND status='pending' AND op IN ('create', 'update'))",
                    [file_id],
                    |r| r.get(0),
                )
                .unwrap_or(false);
            if exists {
                return Ok(0);
            }
        } else if op == "create" {
            let exists: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM upload_queue WHERE file_id=?1 AND status='pending' AND op='create')",
                    [file_id],
                    |r| r.get(0),
                )
                .unwrap_or(false);
            if exists {
                return Ok(0);
            }
        }

        conn.execute(
            "INSERT INTO upload_queue(file_id, op, payload, status, created_at)
             VALUES(?1, ?2, ?3, 'pending', ?4)",
            params![file_id, op, payload, now()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn peek_pending_uploads(&self, limit: usize) -> Result<Vec<UploadTask>> {
        let sql = "SELECT id, file_id, op, payload, status, created_at, attempts, last_error
                   FROM upload_queue WHERE status='pending' ORDER BY id ASC LIMIT ?1";
        let conn = self.conn();
        let mut stmt = conn.prepare_cached(sql)?;
        let tasks = stmt.query_map([limit as i64], upload_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(tasks)
    }

    pub fn list_queue_items(&self, limit: usize) -> Result<Vec<QueueItemView>> {
        let conn = self.conn();
        let sql = "SELECT q.id, q.op, q.file_id, i.name, q.payload, COALESCE(i.size, 0),
                          q.status, q.attempts, q.created_at, q.last_error
                   FROM upload_queue q
                   LEFT JOIN items i ON q.file_id = i.id
                   ORDER BY q.id DESC LIMIT ?1";
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt
            .query_map([limit as i64], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, Option<String>>(3)?,
                    r.get::<_, Option<String>>(4)?,
                    r.get::<_, i64>(5)? as u64,
                    r.get::<_, String>(6)?,
                    r.get::<_, i64>(7)?,
                    r.get::<_, i64>(8)?,
                    r.get::<_, Option<String>>(9)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut items = Vec::with_capacity(rows.len());
        for (id, op, file_id, iname, payload, size, status, attempts, created_at, last_error) in rows {
            let mut fname = iname;
            if fname.is_none() {
                if let Some(ref p_str) = payload {
                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(p_str) {
                        if let Some(n) = val.get("name").and_then(|v| v.as_str()) {
                            fname = Some(n.to_string());
                        }
                    }
                }
            }
            let display_name = match fname {
                Some(name) if name != file_id => format!("{name} ({file_id})"),
                Some(name) => name,
                None => file_id.clone(),
            };

            items.push(QueueItemView {
                id,
                op,
                file_id,
                display_name,
                size_bytes: size,
                status,
                attempts,
                created_at,
                last_error,
            });
        }
        Ok(items)
    }

    pub fn update_upload_status(&self, id: i64, status: &str, error: Option<&str>) -> Result<()> {
        self.conn().execute(
            "UPDATE upload_queue SET status=?2, attempts=attempts+1, last_error=?3 WHERE id=?1",
            params![id, status, error],
        )?;
        Ok(())
    }

    pub fn complete_upload(&self, id: i64) -> Result<()> {
        self.conn().execute("DELETE FROM upload_queue WHERE id=?1", [id])?;
        Ok(())
    }

    pub fn cancel_uploads(&self, file_id: &str) -> Result<()> {
        self.conn().execute("DELETE FROM upload_queue WHERE file_id=?1", [file_id])?;
        Ok(())
    }

    pub fn cancel_pending_updates(&self, file_id: &str) -> Result<()> {
        self.conn().execute(
            "DELETE FROM upload_queue WHERE file_id=?1 AND op='update' AND status='pending'",
            [file_id],
        )?;
        Ok(())
    }

    pub fn pending_uploads_count(&self) -> Result<u64> {
        Ok(self.conn().query_row(
            "SELECT COUNT(*) FROM upload_queue WHERE status='pending'",
            [],
            |r| r.get::<_, i64>(0),
        )? as u64)
    }

    pub fn pending_uploads_summary(&self) -> Result<QueueSummary> {
        let (total, uploads, bytes, trashes, others, failed): (i64, i64, i64, i64, i64, i64) = self.conn().query_row(
            "SELECT
                COUNT(CASE WHEN q.status = 'pending' THEN 1 END),
                COALESCE(SUM(CASE WHEN q.status = 'pending' AND q.op IN ('create', 'update') THEN 1 ELSE 0 END), 0),
                (SELECT COALESCE(SUM(i.size), 0)
                 FROM items i
                 WHERE i.id IN (SELECT DISTINCT file_id FROM upload_queue WHERE status='pending' AND op IN ('create', 'update'))
                ),
                COALESCE(SUM(CASE WHEN q.status = 'pending' AND q.op = 'trash' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN q.status = 'pending' AND q.op NOT IN ('create', 'update', 'trash') THEN 1 ELSE 0 END), 0),
                COUNT(CASE WHEN q.status = 'failed' THEN 1 END)
             FROM upload_queue q",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )?;
        Ok(QueueSummary {
            total: total as u64,
            uploads: uploads as u64,
            upload_bytes: bytes as u64,
            trashes: trashes as u64,
            others: others as u64,
            failed: failed as u64,
        })
    }

    pub fn failed_uploads_count(&self) -> Result<u64> {
        Ok(self.conn().query_row(
            "SELECT COUNT(*) FROM upload_queue WHERE status='failed'",
            [],
            |r| r.get::<_, i64>(0),
        )? as u64)
    }

    pub fn retry_failed_uploads(&self) -> Result<usize> {
        let n = self.conn().execute(
            "UPDATE upload_queue SET status='pending', attempts=0, last_error=NULL WHERE status='failed'",
            [],
        )?;
        Ok(n)
    }

    pub fn retry_upload_ids(&self, ids: &[i64]) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let conn = self.conn();
        let mut count = 0;
        for id in ids {
            count += conn.execute(
                "UPDATE upload_queue SET status='pending', attempts=0, last_error=NULL WHERE id=?1",
                [id],
            )?;
        }
        Ok(count)
    }

    pub fn discard_upload_ids(&self, ids: &[i64]) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let conn = self.conn();
        let mut count = 0;
        for id in ids {
            count += conn.execute(
                "DELETE FROM upload_queue WHERE id=?1",
                [id],
            )?;
        }
        Ok(count)
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
            shortcut_details: None,
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

    #[test]
    fn upload_queue_lifecycle() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.pending_uploads_count().unwrap(), 0);

        let id1 = db.enqueue_upload("file_1", "create", Some("{\"name\":\"test.txt\"}")).unwrap();
        let _id2 = db.enqueue_upload("file_2", "update", None).unwrap();
        assert_eq!(db.pending_uploads_count().unwrap(), 2);

        // Deduplication tests:
        let dup_create = db.enqueue_upload("file_1", "create", None).unwrap();
        assert_eq!(dup_create, 0);
        let dup_update1 = db.enqueue_upload("file_1", "update", None).unwrap();
        assert_eq!(dup_update1, 0);
        let dup_update2 = db.enqueue_upload("file_2", "update", None).unwrap();
        assert_eq!(dup_update2, 0);
        assert_eq!(db.pending_uploads_count().unwrap(), 2);

        // Summary test
        let _ = db.insert_local_item("file_1", None, "test.txt", "text/plain", 1024, 0).unwrap();
        let _ = db.insert_local_item("file_2", None, "file2.txt", "text/plain", 2048, 0).unwrap();
        let summary = db.pending_uploads_summary().unwrap();
        assert_eq!(summary.total, 2);
        assert_eq!(summary.uploads, 2);
        assert_eq!(summary.upload_bytes, 3072);

        let tasks = db.peek_pending_uploads(10).unwrap();
        assert_eq!(tasks.len(), 2);
        assert_eq!(tasks[0].id, id1);
        assert_eq!(tasks[0].file_id, "file_1");
        assert_eq!(tasks[0].op, "create");

        db.complete_upload(id1).unwrap();
        assert_eq!(db.pending_uploads_count().unwrap(), 1);

        let remaining = db.peek_pending_uploads(10).unwrap();
        assert_eq!(remaining[0].file_id, "file_2");

        db.cancel_pending_updates("file_2").unwrap();
        assert_eq!(db.pending_uploads_count().unwrap(), 0);

        // Test retry of failed tasks
        let id3 = db.enqueue_upload("file_3", "create", None).unwrap();
        db.update_upload_status(id3, "failed", Some("network error")).unwrap();
        assert_eq!(db.failed_uploads_count().unwrap(), 1);
        let summary_failed = db.pending_uploads_summary().unwrap();
        assert_eq!(summary_failed.failed, 1);
        let retried = db.retry_failed_uploads().unwrap();
        assert_eq!(retried, 1);
        assert_eq!(db.failed_uploads_count().unwrap(), 0);
        assert_eq!(db.pending_uploads_count().unwrap(), 1);
    }

    #[test]
    fn local_item_management() {
        let db = Db::open_in_memory().unwrap();
        let ino = db.insert_local_item("local_1", Some("ROOT"), "nuevo.txt", "text/plain", 100, 1000).unwrap();
        assert!(ino > 0);

        let item = db.by_id("local_1").unwrap().unwrap();
        assert_eq!(item.name, "nuevo.txt");
        assert_eq!(item.size, 100);
        assert_eq!(item.pinned, false);

        db.set_pinned("local_1", true).unwrap();
        let updated = db.by_id("local_1").unwrap().unwrap();
        assert_eq!(updated.pinned, true);

        db.rename_item("local_1", "renombrado.txt", None).unwrap();
        let renamed = db.by_id("local_1").unwrap().unwrap();
        assert_eq!(renamed.name, "renombrado.txt");

        let lookup = db.lookup_child("ROOT", "renombrado.txt").unwrap().unwrap();
        assert_eq!(lookup.id, "local_1");
    }

    #[test]
    fn test_resolve_relative_path() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_root(&rf("ROOT", "Mi unidad", "", FOLDER_MIME)).unwrap();
        db.upsert(&rf("f1", "Documentos", "ROOT", FOLDER_MIME)).unwrap();
        db.upsert(&rf("f2", "nota.txt", "f1", "text/plain")).unwrap();

        let resolved = db.resolve_relative_path(Path::new("Documentos/nota.txt")).unwrap().unwrap();
        assert_eq!(resolved.id, "f2");
        assert_eq!(resolved.name, "nota.txt");

        // Edge case: root or dot resolves to root
        let root_res = db.resolve_relative_path(Path::new("")).unwrap().unwrap();
        assert_eq!(root_res.id, "ROOT");
        let dot_res = db.resolve_relative_path(Path::new(".")).unwrap().unwrap();
        assert_eq!(dot_res.id, "ROOT");

        let missing = db.resolve_relative_path(Path::new("Documentos/inexistente.txt")).unwrap();
        assert!(missing.is_none());
    }

    #[test]
    fn test_kv_and_replace_local_id() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.kv_get("test_key").unwrap(), None);
        db.kv_set("test_key", "test_val").unwrap();
        assert_eq!(db.kv_get("test_key").unwrap().as_deref(), Some("test_val"));

        db.upsert_root(&rf("ROOT", "Mi unidad", "", FOLDER_MIME)).unwrap();
        let ino = db.insert_local_item("local_123", Some("ROOT"), "borrador.txt", "text/plain", 50, 200).unwrap();
        assert!(db.by_id("local_123").unwrap().is_some());

        db.replace_file_id("local_123", "remote_abc", Some("hash123"), 300).unwrap();
        assert!(db.by_id("local_123").unwrap().is_none());

        let replaced = db.by_id("remote_abc").unwrap().unwrap();
        assert_eq!(replaced.ino, ino);
        assert_eq!(replaced.name, "borrador.txt");
        assert_eq!(replaced.md5.as_deref(), Some("hash123"));
        assert_eq!(replaced.mtime, 300);

        db.remove("remote_abc").unwrap();
        assert!(db.by_id("remote_abc").unwrap().is_none());
    }

    #[test]
    fn test_replace_existing_with_local() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_root(&rf("ROOT", "Mi unidad", "", FOLDER_MIME)).unwrap();
        db.upsert(&rf("remote_doc", "nota.txt", "ROOT", "text/plain")).unwrap();
        let existing = db.by_id("remote_doc").unwrap().unwrap();

        // Local temp file created by editor
        let temp_ino = db.insert_local_item("local_tmp", Some("ROOT"), ".nota.txt.goutputstream", "application/octet-stream", 42, 500).unwrap();

        // Editor renames temp file over existing remote file
        db.replace_existing_with_local("remote_doc", temp_ino, "nota.txt", "ROOT", 42, 500).unwrap();

        // Old inode is gone, new inode now holds the remote ID with updated metadata
        assert!(db.by_ino(existing.ino).unwrap().is_none());
        let updated = db.by_ino(temp_ino).unwrap().unwrap();
        assert_eq!(updated.id, "remote_doc");
        assert_eq!(updated.name, "nota.txt");
        assert_eq!(updated.size, 42);
        assert_eq!(updated.mtime, 500);
        assert_eq!(updated.mime, "text/plain");
    }

    #[test]
    fn test_shortcut_weblink() {
        use crate::drive::ShortcutDetails;

        let db = Db::open_in_memory().unwrap();
        db.upsert_root(&rf("ROOT", "Mi unidad", "", FOLDER_MIME)).unwrap();

        // 1. Folder shortcut
        let mut folder_sc = rf("SC_FOLDER", "Mi Carpeta Atajo", "ROOT", SHORTCUT_MIME);
        folder_sc.web_view_link = Some("https://drive.google.com/file/d/SC_FOLDER/view?usp=drivesdk".into());
        folder_sc.shortcut_details = Some(ShortcutDetails {
            target_id: Some("TARGET_FOLDER_123".into()),
            target_mime_type: Some(FOLDER_MIME.into()),
        });
        db.upsert(&folder_sc).unwrap();
        let item = db.by_id("SC_FOLDER").unwrap().unwrap();
        assert_eq!(
            item.web_link.as_deref(),
            Some("https://drive.google.com/drive/folders/TARGET_FOLDER_123")
        );

        // 2. File shortcut
        let mut file_sc = rf("SC_FILE", "Mi Archivo Atajo", "ROOT", SHORTCUT_MIME);
        file_sc.web_view_link = Some("https://drive.google.com/file/d/SC_FILE/view?usp=drivesdk".into());
        file_sc.shortcut_details = Some(ShortcutDetails {
            target_id: Some("TARGET_FILE_456".into()),
            target_mime_type: Some("application/pdf".into()),
        });
        db.upsert(&file_sc).unwrap();
        let item = db.by_id("SC_FILE").unwrap().unwrap();
        assert_eq!(
            item.web_link.as_deref(),
            Some("https://drive.google.com/open?id=TARGET_FILE_456")
        );
    }

    #[test]
    fn test_pinned_recursive_and_root_items() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_root(&rf("ROOT", "Mi unidad", "", FOLDER_MIME)).unwrap();
        db.upsert(&rf("f_parent", "ParentFolder", "ROOT", FOLDER_MIME)).unwrap();
        db.upsert(&rf("f_child", "ChildFolder", "f_parent", FOLDER_MIME)).unwrap();
        db.upsert(&rf("doc1", "doc1.txt", "f_child", "text/plain")).unwrap();
        db.upsert(&rf("single", "single.txt", "ROOT", "text/plain")).unwrap();

        assert_eq!(db.count_pinned_items().unwrap(), 0);

        // Pin single file
        db.set_pinned("single", true).unwrap();
        assert_eq!(db.count_pinned_items().unwrap(), 1);

        // Pin folder recursively
        let targets = db.set_pinned_recursive("f_parent", true).unwrap();
        // Should return doc1 as download target
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].id, "doc1");

        assert_eq!(db.count_pinned_items().unwrap(), 4); // single, f_parent, f_child, doc1

        // Root pinned items should ONLY return f_parent and single (not f_child or doc1)
        let root_pinned = db.get_root_pinned_items().unwrap();
        assert_eq!(root_pinned.len(), 2);
        assert!(root_pinned.iter().any(|it| it.id == "f_parent" && it.is_folder && it.file_count == 1));
        assert!(root_pinned.iter().any(|it| it.id == "single" && !it.is_folder && it.file_count == 1));

        // Unpin folder recursively
        let _ = db.set_pinned_recursive("f_parent", false).unwrap();
        assert_eq!(db.count_pinned_items().unwrap(), 1); // only single remaining
    }

    #[test]
    fn test_explorer_and_queue_items() {
        let db = Db::open_in_memory().unwrap();
        db.upsert_root(&rf("ROOT", "Mi unidad", "", FOLDER_MIME)).unwrap();
        db.upsert(&rf("fold1", "CarpetaA", "ROOT", FOLDER_MIME)).unwrap();
        db.upsert(&rf("file1", "documento.pdf", "fold1", "application/pdf")).unwrap();

        let explorer_items = db.list_explorer_items(10).unwrap();
        assert_eq!(explorer_items.len(), 2);
        let fold_item = explorer_items.iter().find(|it| it.id == "fold1").unwrap();
        assert_eq!(fold_item.status, LocalFileStatus::Folder);
        assert_eq!(fold_item.rel_path, "CarpetaA");

        let file_item = explorer_items.iter().find(|it| it.id == "file1").unwrap();
        assert_eq!(file_item.status, LocalFileStatus::CloudOnly);
        assert_eq!(file_item.rel_path, "CarpetaA/documento.pdf");
        assert_eq!(file_item.parent_location, "CarpetaA");

        // Queue items test
        db.enqueue_upload("file1", "create", Some("{\"name\":\"documento.pdf\"}")).unwrap();
        let queue_items = db.list_queue_items(10).unwrap();
        assert_eq!(queue_items.len(), 1);
        assert_eq!(queue_items[0].file_id, "file1");
        assert_eq!(queue_items[0].op, "create");
        assert_eq!(queue_items[0].display_name, "documento.pdf (file1)");

        // Resolve relative path with .gdoc.desktop suffix
        db.upsert(&rf("gdoc1", "Apuntes", "ROOT", "application/vnd.google-apps.document")).unwrap();
        let resolved = db.resolve_relative_path(Path::new("Apuntes.gdoc.desktop")).unwrap().unwrap();
        assert_eq!(resolved.id, "gdoc1");
    }
}
