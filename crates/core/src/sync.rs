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

/// Processes pending items in the upload queue (submitting local creates, edits, renames, deletes).
pub async fn process_upload_queue(drive: &Drive, db: &Db, cache: &Cache) -> Result<usize> {
    let tasks = db.peek_pending_uploads(10)?;
    if tasks.is_empty() {
        return Ok(0);
    }
    let mut done = 0;
    for task in tasks {
        let res: Result<()> = async {
            match task.op.as_str() {
                "create" => {
                    let Some(item) = db.by_id(&task.file_id)? else {
                        info!("elemento local '{}' fue eliminado antes de subir; descartando tarea #{}", task.file_id, task.id);
                        db.complete_upload(task.id)?;
                        done += 1;
                        return Ok(());
                    };
                    let parent_id = item.parent_id.as_deref().unwrap_or("root");
                    if parent_id.starts_with("local_") {
                        anyhow::bail!("esperando a que el directorio padre '{}' se suba antes de subir '{}'", parent_id, item.name);
                    }
                    let local_path = cache.path(&task.file_id);
                    info!("subiendo nuevo archivo '{}' desde '{}'", item.name, local_path.display());
                    let remote = if local_path.exists() {
                        drive.upload_file_path(&item.name, parent_id, &item.mime, &local_path).await?
                    } else {
                        drive.create_remote_file(&item.name, parent_id, &item.mime, Vec::new()).await?
                    };
                    let mtime = remote.modified_time.as_deref().and_then(crate::db::parse_rfc3339).unwrap_or(0);
                    let new_cache_path = cache.path(&remote.id);
                    if local_path.exists() {
                        let _ = tokio::fs::rename(&local_path, &new_cache_path).await;
                    }
                    db.replace_file_id(&task.file_id, &remote.id, remote.md5_checksum.as_deref(), mtime)?;
                    let _ = db.cancel_pending_updates(&remote.id);
                    db.complete_upload(task.id)?;
                    done += 1;
                }
                "update" => {
                    let Some(item) = db.by_id(&task.file_id)? else {
                        info!("elemento '{}' no encontrado para actualizar; descartando tarea #{}", task.file_id, task.id);
                        db.complete_upload(task.id)?;
                        done += 1;
                        return Ok(());
                    };
                    if task.file_id.starts_with("local_") {
                        anyhow::bail!("esperando a que se complete la subida inicial de '{}'", task.file_id);
                    }
                    let local_path = cache.path(&task.file_id);
                    if local_path.exists() {
                        let meta = tokio::fs::metadata(&local_path).await?;
                        let data_len = meta.len();
                        info!("subiendo cambios de '{}' ({data_len} bytes)", item.name);
                        let remote = drive.update_file_path(&task.file_id, &item.mime, &local_path).await?;
                        let mtime = remote.modified_time.as_deref().and_then(crate::db::parse_rfc3339).unwrap_or(0);
                        db.update_item_size_mtime(&task.file_id, data_len, mtime)?;
                    }
                    let _ = db.cancel_pending_updates(&task.file_id);
                    db.complete_upload(task.id)?;
                    done += 1;
                }
                "mkdir" => {
                    let Some(item) = db.by_id(&task.file_id)? else {
                        info!("carpeta local '{}' fue eliminada antes de crear; descartando tarea #{}", task.file_id, task.id);
                        db.complete_upload(task.id)?;
                        done += 1;
                        return Ok(());
                    };
                    let parent_id = item.parent_id.as_deref().unwrap_or("root");
                    if parent_id.starts_with("local_") {
                        anyhow::bail!("esperando a que el directorio padre '{}' se cree en Drive antes de crear '{}'", parent_id, item.name);
                    }
                    info!("creando carpeta remota '{}'", item.name);
                    let remote = drive.create_remote_folder(&item.name, parent_id).await?;
                    let mtime = remote.modified_time.as_deref().and_then(crate::db::parse_rfc3339).unwrap_or(0);
                    db.replace_file_id(&task.file_id, &remote.id, None, mtime)?;
                    db.complete_upload(task.id)?;
                    done += 1;
                }
                "trash" => {
                    info!("enviando a la papelera '{}'", task.file_id);
                    if !task.file_id.starts_with("local_") {
                        let _ = drive.trash_remote_file(&task.file_id).await;
                    }
                    db.complete_upload(task.id)?;
                    done += 1;
                }
                "rename" => {
                    if let Some(payload) = &task.payload {
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) {
                            if let Some(new_name) = v.get("name").and_then(|n| n.as_str()) {
                                if !task.file_id.starts_with("local_") {
                                    let _ = drive.rename_remote_file(&task.file_id, new_name).await;
                                }
                            }
                        }
                    }
                    db.complete_upload(task.id)?;
                    done += 1;
                }
                "move" => {
                    if let Some(payload) = &task.payload {
                        if let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) {
                            if let (Some(new_parent), Some(old_parent)) = (
                                v.get("new_parent").and_then(|n| n.as_str()),
                                v.get("old_parent").and_then(|n| n.as_str()),
                            ) {
                                if !task.file_id.starts_with("local_") {
                                    let _ = drive.move_remote_file(&task.file_id, new_parent, old_parent).await;
                                }
                            }
                        }
                    }
                    db.complete_upload(task.id)?;
                    done += 1;
                }
                _ => {
                    db.complete_upload(task.id)?;
                }
            }
            Ok(())
        }.await;

        if let Err(e) = res {
            tracing::warn!("error procesando subida de tarea #{}: {e:#}", task.id);
            let next_status = if task.attempts >= 4 { "failed" } else { "pending" };
            let _ = db.update_upload_status(task.id, next_status, Some(&e.to_string()));
        }
    }
    Ok(done)
}
