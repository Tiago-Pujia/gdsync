mod fs;

use anyhow::{Context, Result};
use clap::Parser;
use fuser::MountOption;
use gdsync_core::cache::Cache;
use gdsync_core::config::{cache_dir, db_path, Config};
use gdsync_core::db::Db;
use gdsync_core::drive::Drive;
use gdsync_core::sync;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

/// gdsync daemon: mounts Google Drive as a local folder with on-demand caching and bidirectional sync.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Mount point (default: the one in config.json, ~/GoogleDrive).
    #[arg(long)]
    mount: Option<PathBuf>,
}

/// A mount point left behind by a crash fails with ENOTCONN; clean it up.
fn prepare_mountpoint(path: &Path) -> Result<()> {
    if let Err(e) = std::fs::metadata(path) {
        if e.raw_os_error() == Some(libc::ENOTCONN) {
            warn!("punto de montaje colgado, desmontando {}", path.display());
            let _ = std::process::Command::new("fusermount3").arg("-uz").arg(path).status();
        }
    }
    std::fs::create_dir_all(path).with_context(|| format!("no se pudo crear {}", path.display()))
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();
    let args = Args::parse();
    let cfg = Config::load()?;
    let mount = args.mount.unwrap_or_else(|| cfg.mount_point.clone());

    let rt = tokio::runtime::Runtime::new()?;
    let db = Arc::new(Db::open(&db_path())?);
    let drive = Arc::new(Drive::new(cfg.clone())?);
    let cache = Arc::new(Cache::new(
        cache_dir().join("blobs"),
        db.clone(),
        drive.clone(),
        cfg.cache_max_gb * 1024 * 1024 * 1024,
    )?);

    let total_space = Arc::new(AtomicU64::new(15 * 1024 * 1024 * 1024)); // 15 GB default
    let used_space = Arc::new(AtomicU64::new(0));

    // Try fetching initial quota from Drive
    if let Ok((used, total)) = rt.block_on(drive.about_storage()) {
        total_space.store(total, Ordering::Relaxed);
        used_space.store(used, Ordering::Relaxed);
        let _ = db.kv_set("storage_total", &total.to_string());
        let _ = db.kv_set("storage_used", &used.to_string());
    }

    rt.block_on(sync::initial_sync(&drive, &db, &cfg))?;

    let upload_notify = Arc::new(tokio::sync::Notify::new());

    // Background worker: upload queue + remote changes polling + quota refresh
    {
        let (drive, db, cache, cfg_clone) = (drive.clone(), db.clone(), cache.clone(), cfg.clone());
        let (total_s, used_s) = (total_space.clone(), used_space.clone());
        let upload_notify = upload_notify.clone();
        let mut sigusr1 = rt.block_on(async {
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1())
        })?;

        rt.spawn(async move {
            let mut tick = 0u64;
            let mut last_auto_retry = std::time::Instant::now();
            loop {
                // Reload configuration dynamically to detect changes in sync_folders or intervals
                let active_cfg = Config::load().unwrap_or_else(|_| cfg_clone.clone());
                let sleep_duration = Duration::from_secs(active_cfg.poll_interval_secs.max(5));

                // 0. Auto-retry failed uploads periodically if enabled
                if active_cfg.auto_retry_secs > 0 && last_auto_retry.elapsed() >= Duration::from_secs(active_cfg.auto_retry_secs) {
                    last_auto_retry = std::time::Instant::now();
                    match db.failed_uploads_count() {
                        Ok(cnt) if cnt > 0 => {
                            match db.retry_failed_uploads() {
                                Ok(n) => info!("reintentando automáticamente {n} tareas fallidas en la cola de sincronización"),
                                Err(e) => warn!("error al reintentar tareas fallidas automáticamente: {e:#}"),
                            }
                        }
                        _ => {}
                    }
                }

                // 1. Process pending local uploads first (drain queue continuously)
                let mut total_uploaded = 0;
                loop {
                    match sync::process_upload_queue(&drive, &db, &cache).await {
                        Ok(0) => break,
                        Ok(n) => {
                            total_uploaded += n;
                            info!("{n} tareas locales subidas a Drive (acumulado: {total_uploaded})");
                        }
                        Err(e) => {
                            warn!("error procesando cola de subida: {e:#}");
                            break;
                        }
                    }
                }
                if total_uploaded > 0 {
                    let gui_active = std::path::Path::new("/tmp/gdsync_gui_ipc").exists()
                        || std::env::var("XDG_RUNTIME_DIR")
                            .map(|d| std::path::Path::new(&d).join("gdsync_gui_ipc").exists())
                            .unwrap_or(false);
                    if !gui_active {
                        let _ = std::process::Command::new("notify-send")
                            .args([
                                "-a", "gdsync",
                                "-i", "gdsync",
                                "gdsync",
                                &format!("{total_uploaded} archivo(s) sincronizado(s) con Google Drive."),
                            ])
                            .spawn();
                    }
                }

                // 2. Poll remote changes
                match sync::poll_changes(&drive, &db, &cache, &active_cfg).await {
                    Ok(0) => {}
                    Ok(n) => info!("{n} cambios remotos aplicados"),
                    Err(e) => warn!("no se pudieron consultar cambios remotos: {e:#}"),
                }

                // 3. Refresh storage quota every ~5 minutes
                tick += 1;
                if tick % 10 == 0 {
                    if let Ok((used, total)) = drive.about_storage().await {
                        total_s.store(total, Ordering::Relaxed);
                        used_s.store(used, Ordering::Relaxed);
                        let _ = db.kv_set("storage_total", &total.to_string());
                        let _ = db.kv_set("storage_used", &used.to_string());
                    }
                }

                tokio::select! {
                    _ = upload_notify.notified() => {}
                    _ = sigusr1.recv() => {
                        info!("comprobación inmediata de cambios remotos solicitada (SIGUSR1)");
                    }
                    _ = tokio::time::sleep(sleep_duration) => {}
                }
            }
        });
    }

    prepare_mountpoint(&mount)?;
    let options = [
        MountOption::FSName("gdsync".into()),
        MountOption::Subtype("gdsync".into()),
        MountOption::RW,
        MountOption::NoAtime,
        MountOption::DefaultPermissions,
    ];
    let fs = fs::DriveFs::new(db, cache, rt.handle().clone(), total_space, used_space, upload_notify);
    let session = fuser::spawn_mount2(fs, &mount, &options)
        .with_context(|| format!("no se pudo montar en {}", mount.display()))?;
    info!("Google Drive montado en modo lectura/escritura en {}", mount.display());

    rt.block_on(async {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
        anyhow::Ok(())
    })?;
    info!("desmontando…");
    drop(session);
    Ok(())
}
