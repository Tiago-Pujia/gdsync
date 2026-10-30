use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use gdsync_core::auth::{self, Token};
use gdsync_core::cache::Cache;
use gdsync_core::config::{cache_dir, db_path, Config};
use gdsync_core::db::{Db, Item};
use gdsync_core::drive::{Drive, FOLDER_MIME};
use std::collections::HashMap;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// gdsync: Google Drive en tu gestor de archivos.
#[derive(Parser)]
#[command(name = "gdsync", author, version, about = "Cliente híbrido de Google Drive para Linux")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Inicia sesión con Google OAuth (la primera vez pasá tus credenciales o completá el prompt).
    Login {
        /// OAuth Client ID de Google Cloud Console
        #[arg(long)]
        client_id: Option<String>,
        /// OAuth Client Secret de Google Cloud Console
        #[arg(long)]
        client_secret: Option<String>,
    },
    /// Lista el contenido remoto de Drive directamente desde la API.
    Ls {
        /// ID o nombre de la carpeta a listar (por defecto la raíz)
        folder: Option<String>,
    },
    /// Muestra el estado del sistema, cuenta, caché y sincronización.
    Status,
    /// Gestión de sincronización selectiva de carpetas.
    Folder {
        #[command(subcommand)]
        sub: FolderCmd,
    },
    /// Consulta o modifica la configuración local.
    Config {
        #[command(subcommand)]
        sub: Option<ConfigCmd>,
    },
    /// Marca un archivo o carpeta como "Disponible sin conexión" (no se borra de la caché).
    Pin {
        /// Ruta al archivo o carpeta
        path: PathBuf,
    },
    /// Desmarca un archivo o carpeta de "Disponible sin conexión" (liberable de la caché).
    Unpin {
        /// Ruta al archivo o carpeta
        path: PathBuf,
    },
    /// Abre el archivo o carpeta seleccionado en la versión web de Google Drive.
    OpenWeb {
        /// Ruta al archivo o carpeta
        path: PathBuf,
    },
    /// Copia el enlace web de Google Drive al portapapeles.
    CopyLink {
        /// Ruta al archivo o carpeta
        path: PathBuf,
    },
    /// Abre la interfaz gráfica de usuario (GUI).
    Gui,
    /// Cierra la sesión activa y revoca el token en Google.
    Logout,
    /// Borra la base de datos local y la caché (ejecutar con el daemon detenido).
    Resync,
}

#[derive(Subcommand)]
enum FolderCmd {
    /// Lista todas las carpetas remotas disponibles en tu Google Drive.
    ListRemote,
    /// Muestra las carpetas configuradas actualmente para sincronizar.
    List,
    /// Agrega una carpeta a la lista de sincronización selectiva (por nombre o ID).
    Add {
        /// Nombre o ID de la carpeta en Google Drive
        folder: String,
    },
    /// Quita una carpeta de la sincronización selectiva.
    Remove {
        /// Nombre o ID de la carpeta
        folder: String,
    },
    /// Restablece la sincronización a toda la unidad completa (root).
    Reset,
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Muestra la configuración actual.
    Show,
    /// Cambia el directorio local de montaje/sincronización.
    SetMount {
        /// Nueva ruta local (ej. ~/GoogleDrive o /media/datos/Drive)
        path: PathBuf,
    },
    /// Cambia el límite máximo de caché de contenido en GB.
    SetCache {
        /// Tamaño en Gigabytes (GB)
        gb: u64,
    },
    /// Cambia el intervalo de consulta de cambios (polling) en segundos.
    SetPoll {
        /// Intervalo en segundos (mínimo 5)
        seconds: u64,
    },
}

fn prompt_input(prompt: &str) -> Result<String> {
    print!("{prompt}: ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let trimmed = line.trim().to_string();
    if trimmed.is_empty() {
        bail!("entrada vacía");
    }
    Ok(trimmed)
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
                (None, None) => match Config::load() {
                    Ok(cfg) => cfg,
                    Err(_) => {
                        println!("No se encontró configuración previa. Ingresá tus credenciales de Google Cloud:");
                        let id = prompt_input("OAuth Client ID")?;
                        let secret = prompt_input("OAuth Client Secret")?;
                        let cfg = Config::new(id, secret);
                        cfg.save()?;
                        cfg
                    }
                },
                _ => bail!("debés pasar tanto --client-id como --client-secret"),
            };

            auth::login(&cfg).await?;
            println!("\n¡Sesión iniciada con éxito!");
            println!("Punto de montaje configurado: {}", cfg.mount_point.display());
            println!("Para montar tu Drive ejecutá: gdsyncd");
        }

        Cmd::Ls { folder } => {
            let drive = Drive::new(Config::load()?)?;
            let target_id = match folder {
                Some(f) => {
                    let matches = drive.search_folder(&f).await?;
                    if matches.is_empty() {
                        bail!("no se encontró ninguna carpeta con el nombre o ID: {f}");
                    }
                    matches[0].id.clone()
                }
                None => drive.get_file("root").await?.id,
            };

            let mut files = drive.list_children(&target_id).await?;
            files.sort_by(|a, b| {
                (b.mime_type == FOLDER_MIME)
                    .cmp(&(a.mime_type == FOLDER_MIME))
                    .then(a.name.cmp(&b.name))
            });

            println!("Contenido ({} elementos):\n", files.len());
            for f in files {
                let icon = if f.mime_type == FOLDER_MIME { "📁" } else { "📄" };
                let size_str = f
                    .size
                    .as_deref()
                    .and_then(|s| s.parse::<u64>().ok())
                    .map(|b| format!(" ({:.2} MB)", b as f64 / 1_000_000.0))
                    .unwrap_or_default();
                println!("{icon} {} [{}] {size_str}", f.name, f.id);
            }
        }

        Cmd::Status => {
            let cfg = Config::load().unwrap_or_else(|_| Config::new("".into(), "".into()));
            println!("=== Estado de gdsync ===");
            println!("Punto de montaje:     {}", cfg.mount_point.display());
            println!(
                "Sesión activa:        {}",
                if Token::load().is_ok() { "Sí (token cargado)" } else { "No iniciada" }
            );
            println!(
                "Sincronización:       {}",
                if cfg.is_syncing_all() {
                    "Toda la unidad (root)".to_string()
                } else {
                    format!("{} carpeta(s) seleccionada(s)", cfg.sync_folders.len())
                }
            );

            if !cfg.is_syncing_all() {
                for (i, f) in cfg.sync_folders.iter().enumerate() {
                    println!("  [{}] {f}", i + 1);
                }
            }

            if db_path().exists() {
                let db = Db::open(&db_path())?;
                println!("Metadatos locales:    {} elementos en base de datos", db.count_items()?);
                println!(
                    "Caché en disco:       {:.2} MB (Límite: {} GB)",
                    db.cache_total()? as f64 / 1_000_000.0,
                    cfg.cache_max_gb
                );
            } else {
                println!("Base de datos local:  Aún no inicializada (iniciá gdsyncd para montar)");
            }
        }

        Cmd::Folder { sub } => match sub {
            FolderCmd::ListRemote => {
                let drive = Drive::new(Config::load()?)?;
                println!("Consultando carpetas en Google Drive...\n");
                let folders = drive.list_all_folders().await?;
                if folders.is_empty() {
                    println!("No se encontraron carpetas.");
                    return Ok(());
                }

                // Map id -> folder for lookup
                let map: HashMap<String, String> = folders.iter().map(|f| (f.id.clone(), f.name.clone())).collect();
                println!("{:<32} {:<30} {}", "ID", "CARPETA", "PADRE");
                println!("{:-<80}", "");
                for f in &folders {
                    let parent_name = f
                        .parents
                        .first()
                        .and_then(|p| map.get(p).cloned())
                        .unwrap_or_else(|| "Mi unidad".into());
                    println!("{:<32} {:<30} {}", f.id, f.name, parent_name);
                }
                println!("\nTotal: {} carpetas", folders.len());
                println!("Podés seleccionar una carpeta con: gdsync folder add <ID_O_NOMBRE>");
            }

            FolderCmd::List => {
                let cfg = Config::load()?;
                if cfg.is_syncing_all() {
                    println!("Sincronización completa activa: Se sincroniza toda la unidad ('root').");
                } else {
                    println!("Carpetas seleccionadas para sincronizar:");
                    for (i, id) in cfg.sync_folders.iter().enumerate() {
                        println!("  {}. ID: {id}", i + 1);
                    }
                }
            }

            FolderCmd::Add { folder } => {
                let drive = Drive::new(Config::load()?)?;
                let mut cfg = Config::load()?;

                let matches = drive.search_folder(&folder).await?;
                if matches.is_empty() {
                    bail!("no se encontró ninguna carpeta remota que coincida con '{folder}'");
                }
                let target = &matches[0];
                if cfg.add_sync_folder(target.id.clone()) {
                    cfg.save()?;
                    println!("Carpeta agregada: '{}' (ID: {})", target.name, target.id);
                    println!("Carpetas activas actualmente: {:?}", cfg.sync_folders);
                } else {
                    println!("La carpeta '{}' ya estaba incluida en la sincronización.", target.name);
                }
            }

            FolderCmd::Remove { folder } => {
                let mut cfg = Config::load()?;
                let prev_all = cfg.is_syncing_all();
                if cfg.remove_sync_folder(&folder) {
                    cfg.save()?;
                    println!("Carpeta '{}' eliminada de la sincronización.", folder);
                    if cfg.is_syncing_all() && !prev_all {
                        println!("La lista quedó vacía: se restableció a sincronizar toda la unidad ('root').");
                    }
                } else {
                    println!("No se encontró la carpeta '{}' en la lista de sincronización.", folder);
                }
            }

            FolderCmd::Reset => {
                let mut cfg = Config::load()?;
                cfg.reset_sync_folders();
                cfg.save()?;
                println!("Sincronización restablecida: Ahora se sincroniza toda la unidad ('root').");
            }
        },

        Cmd::Config { sub } => {
            let mut cfg = Config::load_or_default();
            match sub {
                None | Some(ConfigCmd::Show) => {
                    println!("=== Configuración de gdsync ===");
                    println!("Archivo de config:    {}", Config::path().display());
                    println!("Punto de montaje:     {}", cfg.mount_point.display());
                    println!("Límite de caché:      {} GB", cfg.cache_max_gb);
                    println!("Intervalo de sondeo:  {} s", cfg.poll_interval_secs);
                    println!(
                        "Carpetas sinc.:       {}",
                        if cfg.is_syncing_all() { "Toda la unidad (root)".into() } else { format!("{:?}", cfg.sync_folders) }
                    );
                    if cfg.client_id.is_empty() {
                        println!("Sesión:               No configurada (ejecutá `gdsync login`)");
                    }
                }
                Some(ConfigCmd::SetMount { path }) => {
                    cfg.mount_point = path;
                    cfg.save()?;
                    println!("Punto de montaje actualizado a: {}", cfg.mount_point.display());
                }
                Some(ConfigCmd::SetCache { gb }) => {
                    if gb == 0 {
                        bail!("el tamaño de caché debe ser al menos 1 GB");
                    }
                    cfg.cache_max_gb = gb;
                    cfg.save()?;
                    println!("Límite de caché actualizado a: {} GB", gb);
                }
                Some(ConfigCmd::SetPoll { seconds }) => {
                    if seconds < 5 {
                        bail!("el intervalo mínimo de sondeo es de 5 segundos");
                    }
                    cfg.poll_interval_secs = seconds;
                    cfg.save()?;
                    println!("Intervalo de sondeo actualizado a: {} s", seconds);
                }
            }
        }

        Cmd::Pin { path } => {
            let cfg = Config::load()?;
            let db = Db::open(&db_path())?;
            let item = resolve_item_from_input_path(&cfg, &db, &path)?;
            db.set_pinned(&item.id, true)?;
            println!("✓ '{}' marcado como disponible sin conexión.", item.name);
            if !item.is_dir() {
                let drive = Arc::new(Drive::new(cfg.clone())?);
                let db_arc = Arc::new(Db::open(&db_path())?);
                let cache = Cache::new(
                    cache_dir().join("blobs"),
                    db_arc,
                    drive,
                    cfg.cache_max_gb * 1024 * 1024 * 1024,
                )?;
                println!("  Descargando copia local a la caché...");
                let _ = cache.ensure(&item).await?;
                println!("  Descarga completada con éxito.");
            }
        }

        Cmd::Unpin { path } => {
            let cfg = Config::load()?;
            let db = Db::open(&db_path())?;
            let item = resolve_item_from_input_path(&cfg, &db, &path)?;
            db.set_pinned(&item.id, false)?;
            println!("✓ '{}' desmarcado de disponible sin conexión (espacio local liberable).", item.name);
        }

        Cmd::OpenWeb { path } => {
            let cfg = Config::load()?;
            let db = Db::open(&db_path())?;
            let item = resolve_item_from_input_path(&cfg, &db, &path)?;
            let url = item.web_link.unwrap_or_else(|| format!("https://drive.google.com/open?id={}", item.id));
            println!("Abriendo en el navegador: {url}");
            let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
        }

        Cmd::CopyLink { path } => {
            let cfg = Config::load()?;
            let db = Db::open(&db_path())?;
            let item = resolve_item_from_input_path(&cfg, &db, &path)?;
            let url = item.web_link.unwrap_or_else(|| format!("https://drive.google.com/open?id={}", item.id));
            println!("{url}");
            if let Ok(mut child) = std::process::Command::new("wl-copy").stdin(std::process::Stdio::piped()).spawn() {
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(url.as_bytes());
                }
            } else if let Ok(mut child) = std::process::Command::new("xclip").arg("-selection").arg("clipboard").stdin(std::process::Stdio::piped()).spawn() {
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(url.as_bytes());
                }
            }
        }

        Cmd::Gui => {
            let mut candidates = vec![
                gdsync_core::config::data_dir().join("gui/main.py"),
                PathBuf::from("/usr/local/share/gdsync/gui/main.py"),
                PathBuf::from("/usr/share/gdsync/gui/main.py"),
            ];
            if let Ok(exe) = std::env::current_exe() {
                if let Some(parent) = exe.parent() {
                    candidates.push(parent.join("../../src/gui/main.py"));
                    candidates.push(parent.join("../src/gui/main.py"));
                    candidates.push(parent.join("src/gui/main.py"));
                    candidates.push(parent.join("gui/main.py"));
                }
            }
            candidates.push(PathBuf::from("src/gui/main.py"));
            candidates.push(PathBuf::from("/home/tiago/Proyectos/Google-Drive-Sincronico/src/gui/main.py"));

            let script = candidates.into_iter().find(|p| p.exists());
            if let Some(path) = script {
                let mut child = std::process::Command::new("python3")
                    .arg(&path)
                    .spawn()
                    .with_context(|| format!("falló al iniciar python3 {}", path.display()))?;
                let _ = child.wait();
            } else {
                bail!("no se encontró el script de la interfaz gráfica. Podés ejecutarlo con: python3 src/gui/main.py");
            }
        }

        Cmd::Logout => {
            if let Ok(tok) = Token::load() {
                let _ = auth::revoke(&tok).await;
                let _ = std::fs::remove_file(Token::path());
            }
            println!("Sesión cerrada y token revocado correctamente.");
        }

        Cmd::Resync => {
            for ext in ["", "-wal", "-shm"] {
                let _ = std::fs::remove_file(format!("{}{ext}", db_path().display()));
            }
            let _ = std::fs::remove_dir_all(cache_dir().join("blobs"));
            println!("Estado local y caché borrados con éxito.");
        }
    }
    Ok(())
}

fn resolve_item_from_input_path(cfg: &Config, db: &Db, path: &Path) -> Result<Item> {
    let rel = if path.is_absolute() {
        if let Ok(r) = path.strip_prefix(&cfg.mount_point) {
            r
        } else {
            path
        }
    } else {
        path
    };
    db.resolve_relative_path(rel)?
        .with_context(|| format!("no se encontró '{}' en Google Drive local", path.display()))
}
