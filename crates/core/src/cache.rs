//! On-demand content cache: downloads a file the first time it's opened and
//! evicts least recently used, unpinned files when over the size limit.
use crate::db::{Db, Item};
use crate::drive::Drive;
use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tracing::{info, warn};

#[derive(Default, Debug, Clone)]
pub struct ReconcileStats {
    pub fixed_sizes: usize,
    pub recovered_orphans: usize,
    pub removed_orphans: usize,
    pub removed_phantoms: usize,
    pub removed_part_files: usize,
    pub evicted_bytes: u64,
}

pub struct Cache {
    dir: PathBuf,
    db: Arc<Db>,
    drive: Arc<Drive>,
    max_bytes: Arc<AtomicU64>,
    /// One lock per file id so concurrent opens trigger a single download.
    inflight: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl Cache {
    pub fn new(dir: PathBuf, db: Arc<Db>, drive: Arc<Drive>, max_bytes: u64) -> Result<Self> {
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            db,
            drive,
            max_bytes: Arc::new(AtomicU64::new(max_bytes)),
            inflight: Mutex::new(HashMap::new()),
        })
    }

    pub fn set_max_bytes(&self, max_bytes: u64) {
        self.max_bytes.store(max_bytes, Ordering::Relaxed);
    }

    pub fn max_bytes(&self) -> u64 {
        self.max_bytes.load(Ordering::Relaxed)
    }

    pub fn path(&self, id: &str) -> PathBuf {
        self.dir.join(id)
    }

    /// True if there's a local copy matching the current remote version.
    pub fn is_fresh(&self, item: &Item) -> bool {
        if item.id.starts_with("local_") && self.path(&item.id).exists() {
            return true;
        }
        matches!(self.db.cache_version(&item.id), Ok(Some(v)) if v == item.version())
            && self.path(&item.id).exists()
    }

    /// Returns the path of an up-to-date local copy, downloading it if needed.
    pub async fn ensure(&self, item: &Item) -> Result<PathBuf> {
        let path = self.path(&item.id);
        if self.is_fresh(item) {
            // Auto-heal size in DB if it was 0 or incorrect
            if let Ok(meta) = std::fs::metadata(&path) {
                let actual = meta.len();
                if actual > 0 {
                    let _ = self.db.cache_update_size(&item.id, actual);
                }
            }
            self.db.cache_touch(&item.id)?;
            return Ok(path);
        }
        if item.id.starts_with("local_") {
            if path.exists() {
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                let _ = self.db.cache_put(&item.id, &item.version(), size);
                return Ok(path);
            } else {
                std::fs::File::create(&path)?;
                let _ = self.db.cache_put(&item.id, &item.version(), 0);
                return Ok(path);
            }
        }
        let lock = self
            .inflight
            .lock()
            .unwrap()
            .entry(item.id.clone())
            .or_default()
            .clone();
        let _guard = lock.lock().await;
        if !self.is_fresh(item) {
            info!("descargando {} ({} bytes)", item.name, item.size);
            let res = self.drive.download(&item.id, &path).await;
            if let Err(e) = res {
                self.inflight.lock().unwrap().remove(&item.id);
                return Err(e);
            }
            let size = std::fs::metadata(&path)?.len();
            self.db.cache_put(&item.id, &item.version(), size)?;
        }
        self.inflight.lock().unwrap().remove(&item.id);
        self.evict();
        Ok(path)
    }

    /// Drops the local copy (file deleted or changed remotely).
    pub fn forget(&self, id: &str) {
        let _ = std::fs::remove_file(self.path(id));
        let _ = self.db.cache_remove(id);
    }

    pub fn evict(&self) -> u64 {
        let max_b = self.max_bytes();
        let mut freed_bytes = 0u64;
        let res = (|| -> Result<()> {
            let mut total = self.db.cache_total()?;
            if total <= max_b {
                return Ok(());
            }
            for (id, size) in self.db.cache_evictable()? {
                if total <= max_b {
                    break;
                }
                let path = self.path(&id);
                let real_size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(size);
                let effectively_freed = if real_size > 0 { real_size } else { size };
                // Open handles keep working: unlinking doesn't affect open files on Linux.
                self.forget(&id);
                total = total.saturating_sub(effectively_freed);
                freed_bytes += effectively_freed;
            }
            Ok(())
        })();
        if let Err(e) = res {
            warn!("error liberando caché: {e:#}");
        }
        freed_bytes
    }

    /// Reconciles the on-disk cache blobs with SQLite database entries:
    /// - Purges abandoned `.part` download files.
    /// - Updates missing or 0-size metadata in the database for existing blobs.
    /// - Adopts valid orphaned files or deletes unknown stray blobs.
    /// - Prunes database cache entries whose files no longer exist on disk.
    /// - Enforces the cache limit via `evict()`.
    pub fn reconcile(&self) -> Result<ReconcileStats> {
        let mut stats = ReconcileStats::default();
        let mut disk_files: HashMap<String, u64> = HashMap::new();

        if let Ok(entries) = std::fs::read_dir(&self.dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let Some(name) = path.file_name().and_then(|n| n.to_str()).map(|s| s.to_string()) else {
                    continue;
                };

                // Check for leftover temporary .part files
                if name.ends_with(".part") {
                    let base_id = name.strip_suffix(".part").unwrap_or(&name);
                    let is_inflight = self.inflight.lock().map(|m| m.contains_key(base_id)).unwrap_or(false);
                    if !is_inflight {
                        if std::fs::remove_file(&path).is_ok() {
                            stats.removed_part_files += 1;
                            info!("eliminado archivo temporal de descarga residual: {name}");
                        }
                    }
                    continue;
                }

                if let Ok(meta) = entry.metadata() {
                    if meta.is_file() {
                        disk_files.insert(name, meta.len());
                    }
                }
            }
        }

        // Fetch current cache DB rows
        let cache_entries = self.db.cache_entries()?;
        let db_cache_map: HashMap<String, u64> = cache_entries
            .into_iter()
            .map(|(id, _, size)| (id, size))
            .collect();

        // 1. Sync files on disk into DB
        for (id, disk_size) in &disk_files {
            if let Some(&db_size) = db_cache_map.get(id) {
                if db_size != *disk_size {
                    let _ = self.db.cache_update_size(id, *disk_size);
                    stats.fixed_sizes += 1;
                }
            } else {
                // Not in cache table, check if item exists in items table
                if let Ok(Some(item)) = self.db.by_id(id) {
                    let _ = self.db.cache_put(id, &item.version(), *disk_size);
                    stats.recovered_orphans += 1;
                    info!("recuperado archivo huérfano en caché: {} ({} bytes)", item.name, disk_size);
                } else if !id.starts_with("local_") {
                    // Stray unknown file on disk
                    let _ = std::fs::remove_file(self.path(id));
                    stats.removed_orphans += 1;
                    info!("eliminado archivo desconocido en directorio de caché: {id}");
                }
            }
        }

        // 2. Prune DB cache rows for files that do not exist on disk
        for id in db_cache_map.keys() {
            if !disk_files.contains_key(id) {
                let _ = self.db.cache_remove(id);
                stats.removed_phantoms += 1;
            }
        }

        // 3. Enforce cache eviction
        stats.evicted_bytes = self.evict();

        Ok(stats)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn test_drive() -> Arc<Drive> {
        Arc::new(Drive::new(Config::new("cid".into(), "sec".into())).unwrap())
    }

    #[test]
    fn test_cache_reconcile_and_eviction() {
        let tmp = std::env::temp_dir().join(format!("gdsync_test_{}", rand::random::<u64>()));
        let blobs_dir = tmp.join("blobs");
        std::fs::create_dir_all(&blobs_dir).unwrap();

        let db = Arc::new(Db::open_in_memory().unwrap());
        let drive = test_drive();
        let cache = Cache::new(blobs_dir.clone(), db.clone(), drive, 50).unwrap();

        // 1. Create a dummy file on disk with 40 bytes
        let file_path = blobs_dir.join("file1");
        std::fs::write(&file_path, vec![0u8; 40]).unwrap();
        // Insert in DB with size = 0 (simulating the bug!)
        db.cache_put("file1", "v1", 0).unwrap();

        // 2. Create a leftover .part file
        let part_path = blobs_dir.join("file2.part");
        std::fs::write(&part_path, vec![0u8; 100]).unwrap();

        // 3. Reconcile
        let stats = cache.reconcile().unwrap();
        assert_eq!(stats.fixed_sizes, 1);
        assert_eq!(stats.removed_part_files, 1);
        assert!(!part_path.exists());
        assert_eq!(db.cache_total().unwrap(), 40);

        // 4. Create another file of 40 bytes (total 80 > max 50)
        let file2_path = blobs_dir.join("file2");
        std::fs::write(&file2_path, vec![0u8; 40]).unwrap();
        db.cache_put("file2", "v2", 40).unwrap();
        assert_eq!(db.cache_total().unwrap(), 80);

        // Evict should remove file1 (oldest accessed)
        let freed = cache.evict();
        assert_eq!(freed, 40);
        assert!(!file_path.exists());
        assert!(file2_path.exists());
        assert_eq!(db.cache_total().unwrap(), 40);

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
