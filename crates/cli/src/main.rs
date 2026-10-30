use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use gdsync_core::auth::{self, Token};
use gdsync_core::config::{cache_dir, db_path, Config};
use gdsync_core::db::Db;
use gdsync_core::drive::{Drive, FOLDER_MIME};

/// gdsync: Google Drive in your file manager.
#[derive(Parser)]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Sign in with Google (the first time, pass your OAuth client).
    Login {
        #[arg(long)]
        client_id: Option<String>,
        #[arg(long)]
        client_secret: Option<String>,
    },
    /// List the root of your Drive (straight from the API).
    Ls,
    /// Show configuration and local state.
    Status,
    /// Sign out and revoke access.
    Logout,
    /// Wipe local state; the next daemon start re-downloads the tree. Stop the daemon first.
    Resync,
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Login { client_id, client_secret } => {
            let cfg = match (client_id, client_secret) {
                (Some(id), Some(secret)) => {
                    let mut cfg = Config::load().unwrap_or_else(|_| Config::new(id.clone(), secret.clone()));
                    cfg.client_id = id;
                    cfg.client_secret = secret;
                    cfg.save()?;
                    cfg
                }
                (None, None) => Config::load()?,
                _ => bail!("pasá --client-id y --client-secret juntos"),
            };
            auth::login(&cfg).await?;
            println!("Sesión iniciada. Ahora podés iniciar el daemon: gdsyncd");
        }
        Cmd::Ls => {
            let drive = Drive::new(Config::load()?)?;
            let root = drive.get_file("root").await?;
            let mut files = drive.list_children(&root.id).await?;
            files.sort_by(|a, b| (b.mime_type == FOLDER_MIME).cmp(&(a.mime_type == FOLDER_MIME)).then(a.name.cmp(&b.name)));
            println!("{} ({} elementos)", root.name, files.len());
            for f in files {
                let icon = if f.mime_type == FOLDER_MIME { "📁" } else { "  " };
                println!("{icon} {}", f.name);
            }
        }
        Cmd::Status => {
            let cfg = Config::load()?;
            println!("Punto de montaje: {}", cfg.mount_point.display());
            println!("Sesión:           {}", if Token::load().is_ok() { "iniciada" } else { "no iniciada" });
            if db_path().exists() {
                let db = Db::open(&db_path())?;
                println!("Elementos:        {}", db.count_items()?);
                println!(
                    "Caché:            {:.1} MB de {} GB",
                    db.cache_total()? as f64 / 1e6,
                    cfg.cache_max_gb
                );
            } else {
                println!("Estado local:     vacío (todavía no se inició el daemon)");
            }
        }
        Cmd::Logout => {
            if let Ok(tok) = Token::load() {
                let _ = auth::revoke(&tok).await;
                std::fs::remove_file(Token::path())?;
            }
            println!("Sesión cerrada.");
        }
        Cmd::Resync => {
            for ext in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{ext}", db_path().display()));
            }
            let _ = std::fs::remove_dir_all(cache_dir().join("blobs"));
            println!("Estado local borrado.");
        }
    }
    Ok(())
}
