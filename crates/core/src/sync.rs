//! Keeps the local metadata mirror in step with Drive.
use crate::cache::Cache;
use crate::db::Db;
use crate::drive::Drive;
use anyhow::{Context, Result};
use tracing::info;

const PAGE_TOKEN: &str = "page_token";

/// First run: grab the full tree. The change token is taken *before* listing
/// so nothing that happens during the listing is lost.
pub async fn initial_sync(drive: &Drive, db: &Db) -> Result<()> {
    if db.kv_get(PAGE_TOKEN)?.is_some() {
        return Ok(());
    }
    info!("primera sincronización: descargando el árbol de carpetas…");
    let token = drive.start_page_token().await?;
    db.upsert_root(&drive.get_file("root").await?)?;
    let mut n = 0;
    drive
        .list_all(|files| {
            n += files.len();
            db.upsert_many(&files)?;
            info!("{n} elementos…");
            Ok(())
        })
        .await?;
    db.kv_set(PAGE_TOKEN, &token)?;
    info!("árbol listo: {n} elementos");
    Ok(())
}

/// Applies remote changes since the last poll. Returns how many were applied.
pub async fn poll_changes(drive: &Drive, db: &Db, cache: &Cache) -> Result<usize> {
    let token = db.kv_get(PAGE_TOKEN)?.context("falta la sincronización inicial")?;
    let (changes, next) = drive.changes(&token).await?;
    for c in &changes {
        match &c.file {
            Some(f) if !c.removed && !f.trashed => {
                if let Some(old) = db.by_id(&f.id)? {
                    if old.md5 != f.md5_checksum {
                        cache.forget(&f.id); // stale copy; re-downloaded on next open
                    }
                }
                db.upsert(f)?;
            }
            _ => {
                db.remove(&c.file_id)?;
                cache.forget(&c.file_id);
            }
        }
    }
    db.kv_set(PAGE_TOKEN, &next)?;
    Ok(changes.len())
}
