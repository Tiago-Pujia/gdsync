//! On-demand content cache: downloads a file the first time it's opened and
//! evicts least recently used, unpinned files when over the size limit.
use crate::db::{Db, Item};
use crate::drive::Drive;
use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tracing::{info, warn};

pub struct Cache {
    dir: PathBuf,
    db: Arc<Db>,
    drive: Arc<Drive>,
    max_bytes: u64,
    /// One lock per file id so concurrent opens trigger a single download.
    inflight: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl Cache {
    pub fn new(dir: PathBuf, db: Arc<Db>, drive: Arc<Drive>, max_bytes: u64) -> Result<Self> {
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir, db, drive, max_bytes, inflight: Mutex::new(HashMap::new()) })
    }

    pub fn path(&self, id: &str) -> PathBuf {
        self.dir.join(id)
    }

    /// True if there's a local copy matching the current remote version.
    pub fn is_fresh(&self, item: &Item) -> bool {
        matches!(self.db.cache_version(&item.id), Ok(Some(v)) if v == item.version())
            && self.path(&item.id).exists()
    }

    /// Returns the path of an up-to-date local copy, downloading it if needed.
    pub async fn ensure(&self, item: &Item) -> Result<PathBuf> {
        let path = self.path(&item.id);
        if self.is_fresh(item) {
            self.db.cache_touch(&item.id)?;
            return Ok(path);
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

    fn evict(&self) {
        let res = (|| -> Result<()> {
            let mut total = self.db.cache_total()?;
            if total <= self.max_bytes {
                return Ok(());
            }
            for (id, size) in self.db.cache_evictable()? {
                if total <= self.max_bytes {
                    break;
                }
                // Open handles keep working: unlinking doesn't affect open files on Linux.
                self.forget(&id);
                total = total.saturating_sub(size);
            }
            Ok(())
        })();
        if let Err(e) = res {
            warn!("error liberando caché: {e:#}");
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }
}
