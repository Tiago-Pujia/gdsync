//! Keeps the local metadata mirror in step with Drive.
use crate::cache::Cache;
use crate::config::Config;
use crate::db::Db;
use crate::drive::{Drive, FOLDER_MIME};
use anyhow::{Context, Result};
use tracing::info;

const PAGE_TOKEN: &str = "page_token";

/// First run: grab the full tree or only selected folders.
pub async fn initial_sync(drive: &Drive, db: &Db, cfg: &Config) -> Result<()> {
    if db.kv_get(PAGE_TOKEN)?.is_some() {
        return Ok(());
    }
    info!("primera sincronización: descargando el árbol de carpetas…");
    let token = drive.start_page_token().await?;
    let root = drive.get_file("root").await?;
    let root_id = root.id.clone();
    db.upsert_root(&root)?;

    let mut n = 0;
    if cfg.is_syncing_all() {
        drive
            .list_all(|files| {
                n += files.len();
                db.upsert_many(&files)?;
                info!("{n} elementos…");
                Ok(())
            })
            .await?;
    } else {
        // Sincronización selectiva: solo cargar las carpetas elegidas y su contenido
        for folder_id in &cfg.sync_folders {
            if folder_id == "root" {
                continue;
            }
            if let Ok(mut folder_file) = drive.get_file(folder_id).await {
                // Hacer que la carpeta elegida cuelgue de la raíz local
                folder_file.parents = vec![root_id.clone()];
                db.upsert(&folder_file)?;
                n += 1;

                // Recorrer recursivamente sus subcarpetas y archivos
                let mut queue = vec![folder_file.id];
                while let Some(parent) = queue.pop() {
                    let children = drive.list_children(&parent).await?;
                    for child in &children {
                        if child.mime_type == FOLDER_MIME {
                            queue.push(child.id.clone());
                        }
                    }
                    n += children.len();
                    db.upsert_many(&children)?;
                }
            }
        }
    }

    db.kv_set(PAGE_TOKEN, &token)?;
    info!("árbol listo: {n} elementos sincronizados");
    Ok(())
}

/// Applies remote changes since the last poll. Returns how many were applied.
pub async fn poll_changes(drive: &Drive, db: &Db, cache: &Cache, cfg: &Config) -> Result<usize> {
    let token = db.kv_get(PAGE_TOKEN)?.context("falta la sincronización inicial")?;
    let (changes, next) = drive.changes(&token).await?;
    let syncing_all = cfg.is_syncing_all();

    for c in &changes {
        match &c.file {
            Some(f) if !c.removed && !f.trashed => {
                let relevant = syncing_all
                    || db.by_id(&f.id)?.is_some()
                    || f.parents.iter().any(|p| db.by_id(p).map(|opt| opt.is_some()).unwrap_or(false));

                if relevant {
                    if let Some(old) = db.by_id(&f.id)? {
                        if old.md5 != f.md5_checksum {
                            cache.forget(&f.id); // stale copy; re-downloaded on next open
                        }
                    }
                    db.upsert(f)?;
                }
            }
            _ => {
                if db.by_id(&c.file_id)?.is_some() {
                    db.remove(&c.file_id)?;
                    cache.forget(&c.file_id);
                }
            }
        }
    }
    db.kv_set(PAGE_TOKEN, &next)?;
    Ok(changes.len())
}
