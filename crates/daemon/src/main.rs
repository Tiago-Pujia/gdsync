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
use std::sync::Arc;
use std::time::Duration;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

/// gdsync daemon: mounts Google Drive as a folder.
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
            std::process::Command::new("fusermount3").arg("-uz").arg(path).status()?;
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

    rt.block_on(sync::initial_sync(&drive, &db, &cfg))?;

    {
        let (drive, db, cache, cfg_clone) = (drive.clone(), db.clone(), cache.clone(), cfg.clone());
        let every = Duration::from_secs(cfg.poll_interval_secs.max(5));
        rt.spawn(async move {
            loop {
                match sync::poll_changes(&drive, &db, &cache, &cfg_clone).await {
                    Ok(0) => {}
                    Ok(n) => info!("{n} cambios remotos aplicados"),
                    // Offline or API hiccup: keep serving from the local state.
                    Err(e) => warn!("no se pudieron consultar cambios: {e:#}"),
                }
                tokio::time::sleep(every).await;
            }
        });
    }

    prepare_mountpoint(&mount)?;
    let options = [
        MountOption::FSName("gdsync".into()),
        MountOption::Subtype("gdsync".into()),
        MountOption::RO,
        MountOption::NoAtime,
        MountOption::DefaultPermissions,
    ];
    let fs = fs::DriveFs::new(db, cache, rt.handle().clone());
    let session = fuser::spawn_mount2(fs, &mount, &options)
        .with_context(|| format!("no se pudo montar en {}", mount.display()))?;
    info!("Google Drive montado en {}", mount.display());

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
