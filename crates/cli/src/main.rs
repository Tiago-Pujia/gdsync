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
    /// Inicia sesión con Google OAuth (la primera vez pasá tus credenciales, el archivo JSON o completá el prompt).
    Login {
        /// OAuth Client ID de Google Cloud Console
        #[arg(long)]
        client_id: Option<String>,
        /// OAuth Client Secret de Google Cloud Console
        #[arg(long)]
        client_secret: Option<String>,
        /// Ruta al archivo JSON descargado de Google Cloud Console (client_secret_*.json)
        #[arg(short = 'c', long = "credentials")]
        credentials: Option<PathBuf>,
        /// Archivo JSON de credenciales opcional como argumento posicional
        #[arg(value_name = "CREDENTIALS_JSON")]
        file: Option<PathBuf>,
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
    /// Marca un archivo o carpeta como "Disponible sin conexión" (o lista elementos anclados).
    Pin {
        /// Lista los elementos disponibles sin conexión
        #[arg(short = 'l', long = "list")]
        list: bool,
        /// Ruta(s) al archivo o carpeta
        #[arg(value_name = "PATH")]
        paths: Vec<PathBuf>,
    },
    /// Desmarca un archivo o carpeta de "Disponible sin conexión" (liberable de la caché).
    Unpin {
        /// Ruta(s) al archivo o carpeta
        #[arg(required = true, value_name = "PATH")]
        paths: Vec<PathBuf>,
    },
    /// Abre el archivo o carpeta seleccionado en la versión web de Google Drive.
    #[command(name = "open-web", alias = "openweb")]
    OpenWeb {
        /// Ruta(s) al archivo o carpeta
        #[arg(required = true, value_name = "PATH")]
        paths: Vec<PathBuf>,
    },
    /// Copia el enlace web de Google Drive al portapapeles.
    #[command(name = "copy-link", alias = "copylink")]
    CopyLink {
        /// Ruta(s) al archivo o carpeta
        #[arg(required = true, value_name = "PATH")]
        paths: Vec<PathBuf>,
    },
    /// Crea un enlace simbólico relativo o acceso directo compatible con Google Drive y Linux.
    #[command(name = "symlink", alias = "shortcut")]
    Symlink {
        /// Destino donde se creará el enlace (si se omite, se solicita o se usa el mismo directorio)
        #[arg(short, long)]
        dest: Option<PathBuf>,
        /// Ruta(s) al archivo o carpeta
        #[arg(required = true, value_name = "PATH")]
        paths: Vec<PathBuf>,
    },
    /// Muestra información detallada y estado de sincronización del archivo o carpeta.
    Info {
        /// Ruta(s) al archivo o carpeta
        #[arg(required = true, value_name = "PATH")]
        paths: Vec<PathBuf>,
    },
    /// Abre el gestor de permisos y compartir en Google Drive Web y copia el enlace.
    Share {
        /// Ruta(s) al archivo o carpeta
        #[arg(required = true, value_name = "PATH")]
        paths: Vec<PathBuf>,
    },
    /// Abre el historial de versiones o revisiones del elemento en Google Drive Web.
    History {
        /// Ruta(s) al archivo o carpeta
        #[arg(required = true, value_name = "PATH")]
        paths: Vec<PathBuf>,
    },
    /// Lista las carpetas padre y archivos sueltos disponibles sin conexión.
    #[command(visible_alias = "offline")]
    Pinned,
    /// Abre la interfaz gráfica de usuario (GUI).
    Gui {
        /// Inicia minimizado en la bandeja del sistema
        #[arg(long)]
        minimized: bool,
        /// Inicia minimizado en la bandeja del sistema
        #[arg(long)]
        tray: bool,
        /// Argumentos adicionales para la interfaz gráfica
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
    /// Cierra la sesión activa y revoca el token en Google.
    Logout,
    /// Borra la base de datos local y la caché (ejecutar con el daemon detenido).
    Resync,
    /// Reintenta las tareas fallidas en la cola de sincronización.
    Retry,
    /// Administra el inicio automático con el sistema al iniciar sesión.
    Autostart {
        #[command(subcommand)]
        sub: Option<AutostartCmd>,
    },
}

#[derive(Subcommand)]
enum AutostartCmd {
    /// Muestra el estado del inicio automático.
    Status,
    /// Habilita el inicio automático de la aplicación y/o el servicio.
    Enable {
        /// Habilitar solo la interfaz gráfica (bandeja del sistema)
        #[arg(long)]
        gui: bool,
        /// Habilitar solo el servicio en segundo plano (daemon)
        #[arg(long)]
        daemon: bool,
    },
    /// Deshabilita el inicio automático.
    Disable {
        /// Deshabilitar solo la interfaz gráfica
        #[arg(long)]
        gui: bool,
        /// Deshabilitar solo el servicio en segundo plano
        #[arg(long)]
        daemon: bool,
    },
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
        Cmd::Login { client_id, client_secret, credentials, file } => {
            let json_file = credentials.or(file);
            let (cid, csecret) = if let Some(path) = json_file {
                println!("Leyendo credenciales desde: {}", path.display());
                let (id, secret) = gdsync_core::config::parse_google_credentials_file(&path)?;
                println!("Credenciales de Google importadas correctamente.");
                (Some(id), Some(secret))
            } else {
                (client_id, client_secret)
            };

            let cfg = match (cid, csecret) {
                (Some(id), Some(secret)) => {
                    let mut cfg = Config::load_or_default();
                    cfg.client_id = id;
                    cfg.client_secret = secret;
                    cfg.save()?;
                    cfg
                }
                (None, None) => match Config::load() {
                    Ok(cfg) if !cfg.client_id.is_empty() && !cfg.client_secret.is_empty() => cfg,
                    _ => {
                        println!("No se encontró configuración previa.");
                        let detected = std::env::var("HOME").ok().map(PathBuf::from).and_then(|h| {
                            for sub in ["Descargas", "Downloads"] {
                                let dir = h.join(sub);
                                if let Ok(entries) = std::fs::read_dir(&dir) {
                                    for entry in entries.flatten() {
                                        let path = entry.path();
                                        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                                            if name.starts_with("client_secret_") && name.ends_with(".json") {
                                                return Some(path);
                                            }
                                        }
                                    }
                                }
                            }
                            None
                        });

                        let (id, secret) = if let Some(found_path) = detected {
                            println!("Se detectó automáticamente un archivo de claves en:");
                            println!("  -> {}", found_path.display());
                            print!("¿Deseás usar este archivo de credenciales de Google? [S/n]: ");
                            io::stdout().flush()?;
                            let mut ans = String::new();
                            io::stdin().read_line(&mut ans)?;
                            let ans_trim = ans.trim().to_lowercase();
                            if ans_trim.is_empty() || ans_trim == "s" || ans_trim == "si" || ans_trim == "y" || ans_trim == "yes" {
                                gdsync_core::config::parse_google_credentials_file(&found_path)?
                            } else {
                                print!("Ruta al archivo JSON (o Enter para ingresar ID/Secret manualmente): ");
                                io::stdout().flush()?;
                                let mut line = String::new();
                                io::stdin().read_line(&mut line)?;
                                let trimmed = line.trim();
                                if !trimmed.is_empty() {
                                    gdsync_core::config::parse_google_credentials_file(&PathBuf::from(trimmed))?
                                } else {
                                    let id = prompt_input("OAuth Client ID")?;
                                    let secret = prompt_input("OAuth Client Secret")?;
                                    (id, secret)
                                }
                            }
                        } else {
                            println!("Podés ingresar la ruta a tu archivo JSON de Google (ej. ~/Descargas/client_secret_*.json)");
                            print!("Ruta a archivo JSON (o Enter para ingresar ID/Secret manualmente): ");
                            io::stdout().flush()?;
                            let mut line = String::new();
                            io::stdin().read_line(&mut line)?;
                            let trimmed = line.trim();
                            if !trimmed.is_empty() {
                                let p = PathBuf::from(trimmed);
                                gdsync_core::config::parse_google_credentials_file(&p)?
                            } else {
                                let id = prompt_input("OAuth Client ID")?;
                                let secret = prompt_input("OAuth Client Secret")?;
                                (id, secret)
                            }
                        };
                        let mut cfg = Config::load_or_default();
                        cfg.client_id = id;
                        cfg.client_secret = secret;
                        cfg.save()?;
                        cfg
                    }
                },
                _ => bail!("debés pasar tanto --client-id como --client-secret, o la ruta al archivo JSON descargado"),
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
                let summary = db.pending_uploads_summary().unwrap_or_default();
                if summary.total == 0 {
                    if summary.failed > 0 {
                        println!("Cola de sincronización: 0 pendientes | ⚠️ {} fallida(s) (ejecutá 'gdsync retry')", summary.failed);
                    } else {
                        println!("Cola de sincronización: 0 tareas pendientes (Todo al día)");
                    }
                } else {
                    let size_str = if summary.upload_bytes >= 1_000_000_000 {
                        format!("{:.2} GB", summary.upload_bytes as f64 / 1_000_000_000.0)
                    } else if summary.upload_bytes >= 1_000_000 {
                        format!("{:.2} MB", summary.upload_bytes as f64 / 1_000_000.0)
                    } else if summary.upload_bytes >= 1024 {
                        format!("{:.1} KB", summary.upload_bytes as f64 / 1024.0)
                    } else {
                        format!("{} B", summary.upload_bytes)
                    };

                    if summary.uploads > 0 && summary.trashes > 0 {
                        println!(
                            "Cola de sincronización: {} tarea(s) pendiente(s) ({} subidas: {size_str}, {} en papelera)",
                            summary.total, summary.uploads, summary.trashes
                        );
                    } else if summary.uploads > 0 {
                        println!(
                            "Cola de sincronización: {} archivo(s) por subir ({size_str})",
                            summary.uploads
                        );
                    } else if summary.trashes > 0 {
                        println!(
                            "Cola de sincronización: {} elemento(s) enviándose a la papelera (0 B por subir)",
                            summary.trashes
                        );
                    } else {
                        println!(
                            "Cola de sincronización: {} tarea(s) pendiente(s)",
                            summary.total
                        );
                    }
                    if summary.failed > 0 {
                        println!("⚠️  Tareas fallidas:     {} en cola (ejecutá 'gdsync retry' para reintentar)", summary.failed);
                    }
                }
                if let (Ok(Some(tot)), Ok(Some(used))) = (db.kv_get("storage_total"), db.kv_get("storage_used")) {
                    if let (Ok(tot_b), Ok(used_b)) = (tot.parse::<u64>(), used.parse::<u64>()) {
                        println!(
                            "Cuota de Drive:       {:.2} GB usados de {:.2} GB ({:.1}%)",
                            used_b as f64 / 1_000_000_000.0,
                            tot_b as f64 / 1_000_000_000.0,
                            (used_b as f64 / tot_b.max(1) as f64) * 100.0
                        );
                    }
                }
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
                    println!(
                        "Notificaciones:       {}",
                        if cfg.notifications_enabled { "Activadas" } else { "Desactivadas" }
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

        Cmd::Pinned => {
            let db = Db::open(&db_path())?;
            print_pinned_list(&db)?;
        }

        Cmd::Pin { list, paths } => {
            let db = Db::open(&db_path())?;
            if list || paths.is_empty() {
                print_pinned_list(&db)?;
                return Ok(());
            }

            let cfg = Config::load()?;
            let drive = Arc::new(Drive::new(cfg.clone())?);
            let db_arc = Arc::new(Db::open(&db_path())?);
            let cache = Cache::new(
                cache_dir().join("blobs"),
                db_arc,
                drive,
                cfg.cache_max_gb * 1024 * 1024 * 1024,
            )?;

            for path in paths {
                let item = match resolve_item_from_input_path(&cfg, &db, &path) {
                    Ok(it) => it,
                    Err(e) => {
                        eprintln!("! {e}");
                        notify_desktop("Google Drive", &format!("No se encontró '{}' en Google Drive local.", path.display()), "dialog-warning");
                        continue;
                    }
                };

                if item.is_dir() {
                    notify_desktop("Google Drive", &format!("Sincronizando carpeta '{}' para uso sin conexión...", item.name), "folder-gdrive");
                    println!("Marcando carpeta '{}' y sus contenidos como disponible sin conexión...", item.name);
                    let targets = db.set_pinned_recursive(&item.id, true)?;
                    let total = targets.len();
                    for child in &targets {
                        println!("  Descargando: {}...", child.name);
                        let _ = cache.ensure(child).await;
                    }
                    notify_desktop(
                        "Google Drive",
                        &format!("✓ '{}' ({} archivo{}) ya disponible sin conexión.", item.name, total, if total != 1 { "s" } else { "" }),
                        "emblem-checked",
                    );
                    println!("✓ Carpeta '{}' ({total} archivos) completada.", item.name);
                } else {
                    notify_desktop("Google Drive", &format!("Descargando '{}' para uso sin conexión...", item.name), "folder-gdrive");
                    println!("Descargando '{}' a la caché local...", item.name);
                    db.set_pinned(&item.id, true)?;
                    if !item.is_google_doc() {
                        let _ = cache.ensure(&item).await?;
                    }
                    notify_desktop("Google Drive", &format!("✓ '{}' disponible sin conexión.", item.name), "emblem-checked");
                    println!("✓ '{}' marcado como disponible sin conexión.", item.name);
                }
            }
        }

        Cmd::Unpin { paths } => {
            let cfg = Config::load()?;
            let db = Db::open(&db_path())?;
            for path in paths {
                let item = match resolve_item_from_input_path(&cfg, &db, &path) {
                    Ok(it) => it,
                    Err(e) => {
                        eprintln!("! {e}");
                        continue;
                    }
                };

                if item.is_dir() {
                    db.set_pinned_recursive(&item.id, false)?;
                    notify_desktop("Google Drive", &format!("☁ Espacio local liberado para '{}' y sus carpetas.", item.name), "folder-cloud");
                    println!("✓ '{}' desmarcado (espacio local liberable).", item.name);
                } else {
                    db.set_pinned(&item.id, false)?;
                    notify_desktop("Google Drive", &format!("☁ Espacio local liberado para '{}'.", item.name), "folder-cloud");
                    println!("✓ '{}' desmarcado de disponible sin conexión.", item.name);
                }
            }
        }

        Cmd::OpenWeb { paths } => {
            let cfg = Config::load()?;
            let db = Db::open(&db_path())?;
            for path in paths {
                let item = match resolve_item_from_input_path(&cfg, &db, &path) {
                    Ok(it) => it,
                    Err(e) => {
                        eprintln!("! {e}");
                        continue;
                    }
                };
                let url = if item.is_shortcut() && item.web_link.as_deref().map(|l| l.contains("/file/d/")).unwrap_or(false) {
                    format!("https://drive.google.com/open?id={}", item.id)
                } else {
                    item.web_link.unwrap_or_else(|| format!("https://drive.google.com/open?id={}", item.id))
                };
                println!("Abriendo en el navegador: {url}");
                let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
            }
        }

        Cmd::CopyLink { paths } => {
            let cfg = Config::load()?;
            let db = Db::open(&db_path())?;
            for path in paths {
                let item = match resolve_item_from_input_path(&cfg, &db, &path) {
                    Ok(it) => it,
                    Err(e) => {
                        eprintln!("! {e}");
                        continue;
                    }
                };
                let url = if item.is_shortcut() && item.web_link.as_deref().map(|l| l.contains("/file/d/")).unwrap_or(false) {
                    format!("https://drive.google.com/open?id={}", item.id)
                } else {
                    item.web_link.unwrap_or_else(|| format!("https://drive.google.com/open?id={}", item.id))
                };
                println!("{url}");
                if copy_to_clipboard(&url) {
                    notify_desktop("Google Drive", &format!("Enlace copiado al portapapeles:\n{}", item.name), "edit-copy");
                } else {
                    notify_desktop("Google Drive", &format!("No se pudo copiar al portapapeles:\n{url}"), "dialog-warning");
                }
            }
        }

        Cmd::Symlink { dest, paths } => {
            let cfg = Config::load()?;
            let db = Db::open(&db_path())?;
            for path in paths {
                let item = match resolve_item_from_input_path(&cfg, &db, &path) {
                    Ok(it) => it,
                    Err(e) => {
                        eprintln!("! {e}");
                        continue;
                    }
                };

                let parent_dir = path.parent().unwrap_or(&cfg.mount_point).to_path_buf();
                let target_dest_dir = dest
                    .clone()
                    .or_else(|| pick_directory_dialog(&parent_dir))
                    .unwrap_or(parent_dir);

                let clean_dest = clean_input_path(&target_dest_dir);

                if clean_dest.starts_with(&cfg.mount_point) {
                    let dest_item = match resolve_item_from_input_path(&cfg, &db, &clean_dest) {
                        Ok(it) => it,
                        Err(e) => {
                            eprintln!("! Destino en Google Drive no válido: {e}");
                            continue;
                        }
                    };

                    let parent_id = if dest_item.is_dir() {
                        dest_item.id.clone()
                    } else {
                        dest_item.parent_id.unwrap_or_else(|| "root".into())
                    };

                    let target_id = if item.is_shortcut() {
                        item.shortcut_target_id().unwrap_or(item.id.clone())
                    } else {
                        item.id.clone()
                    };

                    let shortcut_name = format!("Atajo a {}", item.name);
                    let rand_id: u64 = rand::random();
                    let local_id = format!("local_shortcut_{rand_id:016x}");
                    let now = gdsync_core::now();

                    let _ = db.insert_local_item(
                        &local_id,
                        Some(&parent_id),
                        &shortcut_name,
                        gdsync_core::drive::SHORTCUT_MIME,
                        0,
                        now,
                    );
                    let _ = db.kv_set(&format!("link_{local_id}"), &format!("https://drive.google.com/open?id={target_id}"));

                    let payload = serde_json::json!({ "target_id": target_id }).to_string();
                    let _ = db.enqueue_upload(&local_id, "shortcut", Some(&payload));

                    let _ = std::process::Command::new("pkill")
                        .args(["-USR1", "-x", "gdsyncd"])
                        .status();

                    println!("✓ Acceso directo creado en Google Drive: '{}' en {}", shortcut_name, clean_dest.display());
                    notify_desktop(
                        "Google Drive",
                        &format!("✓ Acceso directo creado en Google Drive:\n'{}'", shortcut_name),
                        "emblem-symbolic-link",
                    );
                } else {
                    let source_path = clean_input_path(&path);
                    let mut link_path = clean_dest.join(format!("Atajo a {}", item.name));
                    if link_path.exists() {
                        let mut n = 2;
                        while clean_dest.join(format!("Atajo a {} ({n})", item.name)).exists() {
                            n += 1;
                        }
                        link_path = clean_dest.join(format!("Atajo a {} ({n})", item.name));
                    }

                    match std::os::unix::fs::symlink(&source_path, &link_path) {
                        Ok(_) => {
                            println!("✓ Enlace simbólico creado: {}", link_path.display());
                            notify_desktop(
                                "Google Drive",
                                &format!("✓ Enlace simbólico creado:\n{}", link_path.display()),
                                "emblem-symbolic-link",
                            );
                        }
                        Err(e) => {
                            eprintln!("! Error creando enlace simbólico: {e}");
                            notify_desktop("Google Drive", &format!("No se pudo crear el enlace: {e}"), "dialog-warning");
                        }
                    }
                }
            }
        }

        Cmd::Info { paths } => {
            let cfg = Config::load()?;
            let db = Db::open(&db_path())?;
            for path in paths {
                let item = match resolve_item_from_input_path(&cfg, &db, &path) {
                    Ok(it) => it,
                    Err(e) => {
                        eprintln!("! {e}");
                        continue;
                    }
                };

                let tipo = if item.is_dir() {
                    "📁 Carpeta".to_string()
                } else if item.is_shortcut() {
                    "🔗 Acceso directo (Google Drive Shortcut)".to_string()
                } else if item.is_google_doc() {
                    "📄 Documento de Google (Docs/Sheets/Slides)".to_string()
                } else {
                    "📄 Archivo estándar".to_string()
                };

                let cache_status = if item.is_dir() {
                    "N/A (carpeta)".to_string()
                } else if db.cache_version(&item.id)?.is_some() {
                    "✅ En caché local (disponible sin conexión)".to_string()
                } else {
                    "☁️ Solo en la nube (se descargará al abrir)".to_string()
                };

                let pin_status = if item.pinned {
                    "📌 Mantener siempre en este equipo (Anclado)".to_string()
                } else {
                    "☁️ Liberable de la caché (Automático)".to_string()
                };

                let sync_status = match db.pending_task_for_file(&item.id)? {
                    Some(t) if t.status == "failed" => format!("⚠️ Error al subir ({:?})", t.last_error),
                    Some(t) => format!("⏳ En cola para subida (operación: {})", t.op),
                    None => "✅ Sincronizado con Google Drive".to_string(),
                };

                let rel_path_str = db
                    .resolve_item_relative_path(&item.id)?
                    .map(|p| cfg.mount_point.join(p).display().to_string())
                    .unwrap_or_else(|| path.display().to_string());

                let mut extra = String::new();
                if item.is_shortcut() {
                    if let Some(target_id) = item.shortcut_target_id() {
                        let target_exists = db.by_id(&target_id)?.is_some();
                        extra = format!(
                            "\nDestino:        ID {}\nExiste local:   {}",
                            target_id,
                            if target_exists { "Sí (sincronizado)" } else { "No (abrirá en navegador)" }
                        );
                    }
                }

                let info_text = format!(
                    "=== Información de Google Drive (gdsync) ===\n\
                    Nombre:         {}\n\
                    Tipo:           {}\n\
                    ID de Drive:    {}\n\
                    Ruta local:     {}\n\
                    Tamaño:         {}\n\
                    Disponibilidad: {}\n\
                    Estado caché:   {}\n\
                    Sincronización: {}{}",
                    item.name,
                    tipo,
                    item.id,
                    rel_path_str,
                    format_size(item.size),
                    pin_status,
                    cache_status,
                    sync_status,
                    extra,
                );

                println!("{info_text}");
                show_info_dialog("Google Drive (gdsync) - Información", &info_text);
            }
        }

        Cmd::Share { paths } => {
            let cfg = Config::load()?;
            let db = Db::open(&db_path())?;
            let drive_res = Drive::new(cfg.clone());

            for path in paths {
                let item = match resolve_item_from_input_path(&cfg, &db, &path) {
                    Ok(it) => it,
                    Err(e) => {
                        eprintln!("! {e}");
                        continue;
                    }
                };
                let target_id = if item.is_shortcut() {
                    item.shortcut_target_id().unwrap_or(item.id.clone())
                } else {
                    item.id.clone()
                };

                let url = if item.is_dir() {
                    format!("https://drive.google.com/drive/folders/{target_id}")
                } else {
                    format!("https://drive.google.com/file/d/{target_id}/view")
                };

                println!("=== Compartir y gestionar permisos: {} ===", item.name);
                println!("ID: {}", target_id);
                println!("Enlace: {url}");
                if copy_to_clipboard(&url) {
                    println!("✓ Enlace copiado al portapapeles.");
                }

                // 1. Try to open native GUI modal via IPC or spawn gdsync-gui
                let gui_socket = if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
                    PathBuf::from(runtime_dir).join("gdsync_gui.sock")
                } else {
                    let uid = unsafe { libc::getuid() };
                    PathBuf::from(format!("/tmp/gdsync_gui_{}.sock", uid))
                };

                let mut gui_opened = false;
                if gui_socket.exists() {
                    if let Ok(mut stream) = std::os::unix::net::UnixStream::connect(&gui_socket) {
                        use std::io::Write;
                        let msg = format!("SHARE {}\n", target_id);
                        if stream.write_all(msg.as_bytes()).is_ok() && stream.flush().is_ok() {
                            println!("✓ Abriendo ventana de permisos en gdsync...");
                            gui_opened = true;
                        }
                    }
                }

                if !gui_opened && (std::env::var("DISPLAY").is_ok() || std::env::var("WAYLAND_DISPLAY").is_ok()) {
                    if let Ok(_) = std::process::Command::new("gdsync-gui")
                        .arg("--share")
                        .arg(&target_id)
                        .spawn()
                    {
                        println!("✓ Iniciando gdsync y abriendo ventana de permisos...");
                        gui_opened = true;
                    }
                }

                if !gui_opened {
                    println!("(Interfaz gráfica no iniciada. Podés gestionar los permisos vía web o gdsync-gui)");
                }

                // 2. Also print current permissions in terminal if possible
                if let Ok(ref drive) = drive_res {
                    match drive.list_permissions(&target_id).await {
                        Ok(perms) => {
                            println!("\nPersonas con acceso ({}):", perms.len());
                            for p in perms {
                                let who = p.display_name.as_deref().unwrap_or_else(|| {
                                    if p.perm_type == "anyone" {
                                        "Cualquier persona con el enlace"
                                    } else {
                                        p.email_address.as_deref().unwrap_or("Usuario")
                                    }
                                });
                                let email = p.email_address.as_deref().map(|e| format!(" <{e}>")).unwrap_or_default();
                                println!("  - {who}{email} [{}] ({})", p.role, p.perm_type);
                            }
                        }
                        Err(e) => {
                            eprintln!("! No se pudieron listar los permisos vía API: {e}");
                        }
                    }
                }

                notify_desktop(
                    "Google Drive",
                    &format!("Gestionar permisos: {}", item.name),
                    "document-share",
                );
            }
        }

        Cmd::History { paths } => {
            let cfg = Config::load()?;
            let db = Db::open(&db_path())?;
            for path in paths {
                let item = match resolve_item_from_input_path(&cfg, &db, &path) {
                    Ok(it) => it,
                    Err(e) => {
                        eprintln!("! {e}");
                        continue;
                    }
                };
                let target_id = if item.is_shortcut() {
                    item.shortcut_target_id().unwrap_or(item.id.clone())
                } else {
                    item.id.clone()
                };

                let url = if item.mime.contains("document") {
                    format!("https://docs.google.com/document/d/{target_id}/revisions")
                } else if item.mime.contains("spreadsheet") {
                    format!("https://docs.google.com/spreadsheets/d/{target_id}/revisions")
                } else if item.mime.contains("presentation") {
                    format!("https://docs.google.com/presentation/d/{target_id}/revisions")
                } else if item.is_dir() {
                    format!("https://drive.google.com/drive/folders/{target_id}")
                } else {
                    format!("https://drive.google.com/file/d/{target_id}/view")
                };

                println!("Abriendo historial de versiones en el navegador: {url}");
                let _ = std::process::Command::new("xdg-open").arg(&url).spawn();
                notify_desktop(
                    "Google Drive",
                    &format!("Abriendo historial de versiones en el navegador:\n{}", item.name),
                    "view-history",
                );
            }
        }

        Cmd::Gui { minimized, tray, args } => {
            let mut candidates = vec![
                PathBuf::from("target/release/gdsync-gui"),
                PathBuf::from("target/debug/gdsync-gui"),
                dirs::home_dir().map(|h| h.join(".local/bin/gdsync-gui")).unwrap_or_default(),
                PathBuf::from("/usr/local/bin/gdsync-gui"),
                PathBuf::from("/usr/bin/gdsync-gui"),
            ];
            if let Ok(exe) = std::env::current_exe() {
                if let Some(parent) = exe.parent() {
                    candidates.insert(0, parent.join("gdsync-gui"));
                }
            }

            let bin = candidates.into_iter().find(|p| p.is_file());
            let exe_path = bin.unwrap_or_else(|| PathBuf::from("gdsync-gui"));
            let mut cmd = std::process::Command::new(exe_path);
            if minimized {
                cmd.arg("--minimized");
            }
            if tray {
                cmd.arg("--tray");
            }
            cmd.args(args);
            let mut child = cmd.spawn().with_context(|| "falló al iniciar gdsync-gui")?;
            let _ = child.wait();
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

        Cmd::Retry => {
            if !db_path().exists() {
                println!("No existe base de datos local de gdsync.");
                return Ok(());
            }
            let db = Db::open(&db_path())?;
            let count = db.retry_failed_uploads()?;
            if count == 0 {
                println!("No hay tareas fallidas en la cola de sincronización.");
            } else {
                println!("✓ Se reactivaron {count} tarea(s) fallida(s) para reintentar.");
                let _ = std::process::Command::new("pkill")
                    .args(["-USR1", "-x", "gdsyncd"])
                    .status();
            }
        }

        Cmd::Autostart { sub } => {
            let config_base = gdsync_core::config::config_dir();
            let autostart_dir = config_base
                .parent()
                .map(|p| p.join("autostart"))
                .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".config/autostart"));
            let autostart_desktop = autostart_dir.join("gdsync-gui.desktop");

            match sub.unwrap_or(AutostartCmd::Status) {
                AutostartCmd::Status => {
                    let gui_enabled = autostart_desktop.exists();
                    let daemon_enabled = std::process::Command::new("systemctl")
                        .args(["--user", "is-enabled", "gdsyncd"])
                        .output()
                        .map(|o| String::from_utf8_lossy(&o.stdout).contains("enabled"))
                        .unwrap_or(false);

                    println!("=== Estado de inicio automático al iniciar sesión ===");
                    println!("Interfaz gráfica (Bandeja): {}", if gui_enabled { "Habilitada (se inicia minimizada)" } else { "Deshabilitada" });
                    println!("Servicio daemon (gdsyncd):  {}", if daemon_enabled { "Habilitado (systemd user)" } else { "Deshabilitado" });
                    println!("\nComandos útiles:");
                    println!("  gdsync autostart enable             # Habilita ambos (GUI y daemon)");
                    println!("  gdsync autostart enable --gui       # Habilita solo la GUI");
                    println!("  gdsync autostart enable --daemon    # Habilita solo el daemon");
                    println!("  gdsync autostart disable            # Deshabilita ambos");
                    println!("  gdsync autostart disable --gui      # Deshabilita solo la GUI");
                    println!("  gdsync autostart disable --daemon   # Deshabilita solo el daemon");
                }
                AutostartCmd::Enable { gui, daemon } => {
                    let both = !gui && !daemon;
                    if both || gui {
                        let _ = std::fs::create_dir_all(&autostart_dir);
                        let content = "[Desktop Entry]\n\
                            Type=Application\n\
                            Name=gdsync\n\
                            GenericName=Cliente de Google Drive\n\
                            Comment=Google Drive sincronizado en tu gestor de archivos\n\
                            Exec=gdsync gui --minimized\n\
                            Icon=gdsync\n\
                            Terminal=false\n\
                            Categories=Network;FileTransfer;Utility;\n\
                            StartupNotify=false\n\
                            X-GNOME-Autostart-enabled=true\n";
                        std::fs::write(&autostart_desktop, content)
                            .with_context(|| format!("falló al escribir {}", autostart_desktop.display()))?;
                        println!("✓ Inicio automático de la interfaz gráfica habilitado.");
                    }
                    if both || daemon {
                        let res = std::process::Command::new("systemctl")
                            .args(["--user", "enable", "gdsyncd"])
                            .status();
                        match res {
                            Ok(s) if s.success() => println!("✓ Servicio systemd de usuario (gdsyncd) habilitado."),
                            _ => println!("! No se pudo habilitar el servicio en systemd (verificá que el archivo de servicio esté instalado)."),
                        }
                    }
                }
                AutostartCmd::Disable { gui, daemon } => {
                    let both = !gui && !daemon;
                    if both || gui {
                        if autostart_desktop.exists() {
                            let _ = std::fs::remove_file(&autostart_desktop);
                        }
                        println!("✓ Inicio automático de la interfaz gráfica deshabilitado.");
                    }
                    if both || daemon {
                        let _ = std::process::Command::new("systemctl")
                            .args(["--user", "disable", "gdsyncd"])
                            .status();
                        println!("✓ Servicio systemd de usuario (gdsyncd) deshabilitado.");
                    }
                }
            }
        }
    }
    Ok(())
}

fn decode_percent(s: &str) -> String {
    let mut bytes = Vec::new();
    let mut chars = s.bytes();
    while let Some(b) = chars.next() {
        if b == b'%' {
            if let (Some(h1), Some(h2)) = (chars.next(), chars.next()) {
                let hex_str = [h1, h2];
                if let Ok(hex) = std::str::from_utf8(&hex_str) {
                    if let Ok(val) = u8::from_str_radix(hex, 16) {
                        bytes.push(val);
                        continue;
                    }
                }
            }
        }
        bytes.push(b);
    }
    String::from_utf8_lossy(&bytes).to_string()
}

fn clean_input_path(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(stripped) = s.strip_prefix("file://") {
        PathBuf::from(decode_percent(stripped))
    } else {
        path.to_path_buf()
    }
}

fn copy_to_clipboard(text: &str) -> bool {
    // 1. KDE Plasma / Klipper vía DBus (Wayland y X11)
    for cmd in ["qdbus6", "qdbus"] {
        if let Ok(status) = std::process::Command::new(cmd)
            .args(["org.kde.klipper", "/klipper", "setClipboardContents", text])
            .status()
        {
            if status.success() {
                return true;
            }
        }
    }

    if let Ok(status) = std::process::Command::new("gdbus")
        .args([
            "call",
            "--session",
            "--dest",
            "org.kde.klipper",
            "--object-path",
            "/klipper",
            "--method",
            "org.kde.klipper.klipper.setClipboardContents",
            text,
        ])
        .status()
    {
        if status.success() {
            return true;
        }
    }

    // 2. Wayland wl-copy
    if let Ok(mut child) = std::process::Command::new("wl-copy").stdin(std::process::Stdio::piped()).spawn() {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
            return true;
        }
    }

    // 3. X11 xclip
    if let Ok(mut child) = std::process::Command::new("xclip").arg("-selection").arg("clipboard").stdin(std::process::Stdio::piped()).spawn() {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
            return true;
        }
    }

    // 4. X11 xsel
    if let Ok(mut child) = std::process::Command::new("xsel").args(["--clipboard", "--input"]).stdin(std::process::Stdio::piped()).spawn() {
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
            return true;
        }
    }

    false
}

fn notify_desktop(title: &str, message: &str, icon: &str) {
    if let Ok(cfg) = Config::load() {
        if !cfg.notifications_enabled {
            return;
        }
    }
    let _ = std::process::Command::new("notify-send")
        .args(["-a", "gdsync", "-i", icon, title, message])
        .spawn();
}

fn show_info_dialog(title: &str, text: &str) {
    if std::env::var("DISPLAY").is_ok() || std::env::var("WAYLAND_DISPLAY").is_ok() {
        if let Ok(mut child) = std::process::Command::new("kdialog")
            .args(["--title", title, "--msgbox", text])
            .spawn()
        {
            let _ = child.wait();
            return;
        }
        if let Ok(mut child) = std::process::Command::new("zenity")
            .args(["--info", &format!("--title={title}"), &format!("--text={text}")])
            .spawn()
        {
            let _ = child.wait();
            return;
        }
    }
    notify_desktop(title, text, "dialog-information");
}

fn pick_directory_dialog(start_dir: &Path) -> Option<PathBuf> {
    if std::env::var("DISPLAY").is_ok() || std::env::var("WAYLAND_DISPLAY").is_ok() {
        if let Ok(output) = std::process::Command::new("kdialog")
            .args(["--getexistingdirectory", &start_dir.to_string_lossy(), "Seleccionar carpeta destino para el acceso directo"])
            .output()
        {
            if output.status.success() {
                let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !s.is_empty() {
                    return Some(clean_input_path(Path::new(&s)));
                }
            }
        }
        if let Ok(output) = std::process::Command::new("zenity")
            .args(["--file-selection", "--directory", "--title=Seleccionar carpeta destino"])
            .output()
        {
            if output.status.success() {
                let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !s.is_empty() {
                    return Some(clean_input_path(Path::new(&s)));
                }
            }
        }
    }
    None
}

fn resolve_item_from_input_path(cfg: &Config, db: &Db, raw_path: &Path) -> Result<Item> {
    db.resolve_path_or_id(&cfg.mount_point, &raw_path.to_string_lossy())?
        .with_context(|| format!("no se encontró '{}' en Google Drive local", raw_path.display()))
}

fn format_size(bytes: u64) -> String {
    let mut val = bytes as f64;
    for unit in ["B", "KB", "MB", "GB", "TB"] {
        if val < 1024.0 {
            return format!("{:.1} {}", val, unit);
        }
        val /= 1024.0;
    }
    format!("{:.1} PB", val)
}

fn print_pinned_list(db: &Db) -> Result<()> {
    let items = db.get_root_pinned_items()?;
    println!("=== Elementos disponibles sin conexión (anclados) ===");
    if items.is_empty() {
        println!("No hay carpetas ni archivos anclados actualmente.\n");
        println!("Para mantener carpetas o archivos sin conexión, podés:");
        println!("  - Clic derecho en Dolphin -> 'Mantener siempre en este equipo'");
        println!("  - Ejecutar: gdsync pin <ruta>");
        return Ok(());
    }

    let folders_count = items.iter().filter(|it| it.is_folder).count();
    let files_count = items.iter().filter(|it| !it.is_folder).count();
    let total_bytes: u64 = items.iter().map(|it| it.total_size).sum();
    let total_files: u64 = items.iter().map(|it| it.file_count).sum();

    println!(
        "Resumen: {} carpeta(s) padre, {} archivo(s) suelto(s) ({} archivos totales, {})\n",
        folders_count,
        files_count,
        total_files,
        format_size(total_bytes)
    );

    let header = format!(
        "{:<10} {:<42} {:<12} {:<12} {}",
        "TIPO", "UBICACIÓN / NOMBRE", "ARCHIVOS", "TAMAÑO", "ESTADO EN CACHÉ"
    );
    println!("{header}");
    println!("{}", "-".repeat(header.len().max(80)));

    for it in &items {
        let tipo = if it.is_folder { "📁 Carpeta" } else { "📄 Archivo" };
        let mut ruta = it.full_path.clone();
        if ruta.len() > 40 {
            ruta = format!("...{}", &ruta[ruta.len() - 37..]);
        }

        let cnt_str = if it.is_folder {
            format!("{} arch.", it.file_count)
        } else {
            "1 arch.".to_string()
        };

        let cache_str = if it.is_folder {
            let pct = if it.file_count > 0 {
                (it.cached_count as f64 / it.file_count as f64 * 100.0) as u64
            } else {
                100
            };
            format!("{}/{} en caché ({}%)", it.cached_count, it.file_count, pct)
        } else if it.cached_count == 1 {
            "En caché (100%)".to_string()
        } else {
            "Pendiente".to_string()
        };

        let size_str = format_size(it.total_size);
        println!("{:<10} {:<42} {:<12} {:<12} {}", tipo, ruta, cnt_str, size_str, cache_str);
    }

    println!("\nComandos útiles:");
    println!("  gdsync pin <ruta>      # Mantener carpeta o archivo sin conexión");
    println!("  gdsync unpin <ruta>    # Liberar espacio local (desanclar)");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_size() {
        assert_eq!(format_size(500), "500.0 B");
        assert_eq!(format_size(1024), "1.0 KB");
        assert_eq!(format_size(1024 * 1024), "1.0 MB");
        assert_eq!(format_size(5 * 1024 * 1024 * 1024), "5.0 GB");
    }

    #[test]
    fn test_clean_input_path() {
        assert_eq!(
            clean_input_path(Path::new("file:///home/user/GoogleDrive/test%20file.txt")),
            PathBuf::from("/home/user/GoogleDrive/test file.txt")
        );
        assert_eq!(
            clean_input_path(Path::new("/home/user/GoogleDrive/normal.txt")),
            PathBuf::from("/home/user/GoogleDrive/normal.txt")
        );
    }
}
