use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arboard::Clipboard;
use gdsync_core::config::{self, Config};
use gdsync_core::db::{Db, ExplorerItem, LocalFileStatus, PinnedRootItem};
use gdsync_core::drive::Drive;
use slint::{Color, ModelRc, VecModel};

use crate::desktop;
use crate::notifications::{format_bytes, NotificationManager};
use crate::workers;
use crate::{AppWindow, FileEntry, FolderNode, LogEntry, PinnedItemEntry, QueueEntry};

pub struct GuiController {
    app: slint::Weak<AppWindow>,
    db: Arc<Db>,
    cfg: Arc<Mutex<Config>>,
    drive: Arc<Mutex<Option<Arc<Drive>>>>,
    notifications: Arc<Mutex<NotificationManager>>,
    tray_status: Arc<Mutex<String>>,
    raw_explorer_items: Arc<Mutex<Vec<ExplorerItem>>>,
    raw_pinned_items: Arc<Mutex<Vec<PinnedRootItem>>>,
    active_search_query: Arc<Mutex<String>>,
    active_filter_idx: Arc<Mutex<i32>>,
    pinned_search_query: Arc<Mutex<String>>,
    last_quota_fetch: Arc<Mutex<Option<std::time::Instant>>>,
}

impl GuiController {
    pub fn new(
        app: slint::Weak<AppWindow>,
        db: Arc<Db>,
        cfg: Arc<Mutex<Config>>,
        drive: Arc<Mutex<Option<Arc<Drive>>>>,
        notifications: Arc<Mutex<NotificationManager>>,
        tray_status: Arc<Mutex<String>>,
    ) -> Self {
        Self {
            app,
            db,
            cfg,
            drive,
            notifications,
            tray_status,
            raw_explorer_items: Arc::new(Mutex::new(Vec::new())),
            raw_pinned_items: Arc::new(Mutex::new(Vec::new())),
            active_search_query: Arc::new(Mutex::new(String::new())),
            active_filter_idx: Arc::new(Mutex::new(0)),
            pinned_search_query: Arc::new(Mutex::new(String::new())),
            last_quota_fetch: Arc::new(Mutex::new(None)),
        }
    }

    pub fn get_drive(&self) -> Option<Arc<Drive>> {
        let mut guard = self.drive.lock().unwrap();
        if guard.is_none() {
            if let Ok(d) = Drive::new(self.cfg.lock().unwrap().clone()) {
                *guard = Some(Arc::new(d));
            }
        }
        guard.clone()
    }

    pub fn init_and_bind(self: Arc<Self>) {
        let Some(app) = self.app.upgrade() else { return };

        // 1. Initial configuration load into Slint UI
        {
            let cfg = self.cfg.lock().unwrap();
            app.set_mount_point(cfg.mount_point.to_string_lossy().to_string().into());
            app.set_cache_size_gb(cfg.cache_max_gb.to_string().into());
            app.set_poll_interval_secs(cfg.poll_interval_secs.to_string().into());
            app.set_client_id(cfg.client_id.clone().into());
            app.set_client_secret(cfg.client_secret.clone().into());
            app.set_notify_enabled(cfg.notifications_enabled);
            app.set_resident_notify(cfg.persistent_upload_notify);
            app.set_close_to_tray(cfg.close_to_tray);
            app.set_autostart_gui(desktop::is_autostart_gui());
            app.set_autostart_daemon(desktop::is_autostart_daemon());
        }

        // 2. Set up Dolphin Places bookmark
        {
            let mount_point = self.cfg.lock().unwrap().mount_point.clone();
            desktop::ensure_dolphin_bookmark(&mount_point);
        }

        // 3. Bind Slint callbacks
        self.bind_callbacks(&app);

        // 4. Initial data loads
        self.refresh_explorer();
        self.refresh_selective_folders();
        self.refresh_pinned_items();
        self.refresh_logs();
        self.refresh_periodic();

        // 5. Setup periodic timer (every 2.5 seconds)
        let this = self.clone();
        let timer = slint::Timer::default();
        timer.start(
            slint::TimerMode::Repeated,
            Duration::from_millis(2500),
            move || {
                this.refresh_periodic();
            },
        );
        // Leak timer so it stays active for the lifetime of the application
        Box::leak(Box::new(timer));
    }

    fn bind_callbacks(self: &Arc<Self>, app: &AppWindow) {
        // Toggle Daemon
        {
            let this = self.clone();
            app.on_toggle_daemon(move || {
                let running = desktop::is_daemon_running();
                let mount = this.cfg.lock().unwrap().mount_point.clone();
                if running {
                    desktop::stop_daemon(&mount);
                    this.show_toast("Servicio Detenido", "gdsyncd se ha detenido y desmontado.", false);
                } else {
                    desktop::start_daemon();
                    this.show_toast("Servicio Iniciado", "gdsyncd se está ejecutando.", false);
                }
                this.refresh_periodic();
            });
        }

        // Open Dolphin
        {
            let this = self.clone();
            app.on_open_dolphin(move || {
                let mount = this.cfg.lock().unwrap().mount_point.clone();
                desktop::open_path_in_file_manager(&mount);
            });
        }

        // Sync Now
        {
            let this = self.clone();
            app.on_sync_now(move || {
                *this.last_quota_fetch.lock().unwrap() = None;
                this.show_toast("Sincronización", "Consultando cambios en Google Drive...", false);
                let this = this.clone();
                if let Some(drive) = this.get_drive() {
                    tokio::spawn(async move {
                        let _ = drive.start_page_token().await;
                        slint::invoke_from_event_loop(move || {
                            this.refresh_periodic();
                            this.refresh_explorer();
                        }).ok();
                    });
                }
            });
        }

        // Toggle Theme
        app.on_toggle_theme(|| {
            // Slint Theme.is-dark already toggled on UI side
        });

        // Show Account
        {
            let app_weak = self.app.clone();
            app.on_show_account(move || {
                if let Some(w) = app_weak.upgrade() {
                    w.set_active_tab(4);
                }
            });
        }

        // Explorer search & filter
        {
            let this = self.clone();
            app.on_explorer_search(move |q| {
                *this.active_search_query.lock().unwrap() = q.to_string();
                this.update_explorer_view_models();
            });
        }
        {
            let this = self.clone();
            app.on_explorer_filter(move |f| {
                *this.active_filter_idx.lock().unwrap() = f;
                this.update_explorer_view_models();
            });
        }
        {
            let this = self.clone();
            app.on_reload_files(move || {
                this.refresh_explorer();
                this.show_toast("Actualizado", "Lista de archivos refrescada desde SQLite.", false);
            });
        }

        // Toggle Pin
        {
            let this = self.clone();
            app.on_toggle_pin(move |id, should_pin| {
                let id_str = id.to_string();
                let this_clone = this.clone();
                if let Ok(unpinned_files) = this.db.set_pinned_recursive(&id_str, should_pin) {
                    if should_pin {
                        this.show_toast("Anclado Offline", "El elemento permanecerá disponible sin conexión.", false);
                        // Background download if uncached
                        if !unpinned_files.is_empty() {
                            if let Some(drive) = this.get_drive() {
                                let db = this.db.clone();
                                tokio::spawn(async move {
                                    let cache_dir = config::cache_dir();
                                    for item in unpinned_files {
                                        let dest = cache_dir.join(&item.id);
                                        if !dest.exists() {
                                            if drive.download(&item.id, &dest).await.is_ok() {
                                                let _ = db.cache_put(&item.id, &item.version(), item.size);
                                            }
                                        }
                                    }
                                });
                            }
                        }
                    } else {
                        this.show_toast("Espacio Liberable", "El elemento ya no está anclado permanentemente.", false);
                    }
                    this_clone.refresh_explorer();
                    this_clone.refresh_pinned_items();
                    this_clone.refresh_periodic();
                }
            });
        }

        // Copy Link
        {
            let this = self.clone();
            app.on_copy_link(move |id| {
                let url = format!("https://drive.google.com/file/d/{}/view", id);
                if let Ok(mut cb) = Clipboard::new() {
                    let _ = cb.set_text(&url);
                    this.show_toast("Enlace Copiado", "URL de Google Drive copiada al portapapeles.", false);
                } else {
                    desktop::open_web_url(&url);
                }
            });
        }

        // Open Web
        app.on_open_web(move |id| {
            let url = if id.is_empty() {
                "https://drive.google.com/drive/my-drive".to_string()
            } else {
                format!("https://drive.google.com/file/d/{}/view", id)
            };
            desktop::open_web_url(&url);
        });

        // Open Local
        {
            let this = self.clone();
            app.on_open_local(move |loc| {
                let loc_str = loc.to_string();
                let mount = this.cfg.lock().unwrap().mount_point.clone();
                let full = if loc_str.starts_with('/') {
                    PathBuf::from(&loc_str)
                } else {
                    mount.join(loc_str)
                };
                desktop::open_path_in_file_manager(&full);
            });
        }

        // Queue Actions
        {
            let this = self.clone();
            app.on_retry_queue_item(move |id| {
                let _ = this.db.retry_upload_ids(&[id as i64]);
                this.show_toast("Reintentando", "Tarea encolada nuevamente para subida.", false);
                this.refresh_periodic();
            });
        }
        {
            let this = self.clone();
            app.on_cancel_queue_item(move |id| {
                let _ = this.db.discard_upload_ids(&[id as i64]);
                this.show_toast("Cancelado", "Tarea eliminada de la cola.", false);
                this.refresh_periodic();
            });
        }
        {
            let this = self.clone();
            app.on_retry_all_queue(move || {
                let count = this.db.retry_failed_uploads().unwrap_or(0);
                this.show_toast("Reintento general", &format!("{} tareas fallidas reintentadas.", count), false);
                this.refresh_periodic();
            });
        }

        // Selective Sync Actions
        {
            let this = self.clone();
            app.on_toggle_folder(move |id, checked| {
                let id_str = id.to_string();
                let mut cfg = this.cfg.lock().unwrap();
                if checked {
                    cfg.add_sync_folder(id_str);
                } else {
                    cfg.remove_sync_folder(&id_str);
                }
                let _ = cfg.save();
                this.refresh_selective_folders();
            });
        }
        {
            let this = self.clone();
            app.on_sync_all_folders(move || {
                let mut cfg = this.cfg.lock().unwrap();
                cfg.reset_sync_folders();
                let _ = cfg.save();
                this.show_toast("Sincronización Total", "Se sincronizarán todas las carpetas ('root').", false);
                this.refresh_selective_folders();
            });
        }
        {
            let this = self.clone();
            app.on_save_folder_selection(move || {
                let cfg = this.cfg.lock().unwrap();
                let _ = cfg.save();
                this.show_toast("Guardado", "Configuración de sincronización selectiva guardada.", false);
            });
        }

        // Account Actions
        {
            let app_weak = self.app.clone();
            app.on_logout(move || {
                if let Some(w) = app_weak.upgrade() {
                    w.set_confirm_title("¿Cerrar Sesión?".into());
                    w.set_confirm_message("Se desconectará tu cuenta de Google y se eliminarán los tokens locales.".into());
                    w.set_confirm_btn_text("Cerrar Sesión".into());
                    w.set_confirm_is_danger(true);
                    w.set_confirm_action_tag("logout".into());
                    w.set_confirm_modal_open(true);
                }
            });
        }
        {
            let this = self.clone();
            app.on_import_credentials_json(move || {
                if let Some(path) = rfd::FileDialog::new()
                    .add_filter("JSON de Google Cloud", &["json"])
                    .pick_file()
                {
                    match workers::parse_google_credentials_json(&path) {
                        Ok((cid, secret)) => {
                            let mut cfg = this.cfg.lock().unwrap();
                            cfg.client_id = cid.clone();
                            cfg.client_secret = secret.clone();
                            let _ = cfg.save();
                            if let Some(w) = this.app.upgrade() {
                                w.set_client_id(cid.into());
                                w.set_client_secret(secret.into());
                            }
                            this.show_toast("Credenciales importadas", "Archivo JSON de Google Cloud cargado con éxito.", false);
                        }
                        Err(e) => {
                            this.show_error("Error al importar JSON", &e.to_string());
                        }
                    }
                }
            });
        }
        {
            let this = self.clone();
            app.on_save_credentials(move |id, secret| {
                let mut cfg = this.cfg.lock().unwrap();
                cfg.client_id = id.to_string();
                cfg.client_secret = secret.to_string();
                let _ = cfg.save();
                this.show_toast("Guardado", "Credenciales OAuth 2.0 guardadas en config.json.", false);
            });
        }
        {
            let this = self.clone();
            app.on_login_oauth(move || {
                let (cid, secret) = {
                    let cfg = this.cfg.lock().unwrap();
                    (cfg.client_id.clone(), cfg.client_secret.clone())
                };
                if cid.is_empty() || secret.is_empty() {
                    this.show_error("Faltan Credenciales", "Ingresá o importá tu Client ID y Client Secret antes de iniciar sesión.");
                    return;
                }
                if let Some(w) = this.app.upgrade() {
                    w.set_is_logging_in(true);
                }
                this.show_toast("Autenticación iniciada", "Completá el inicio de sesión en tu navegador web.", false);
                let this = this.clone();
                tokio::spawn(async move {
                    let res = workers::run_oauth_login(&cid, &secret).await;
                    slint::invoke_from_event_loop(move || {
                        if let Some(w) = this.app.upgrade() {
                            w.set_is_logging_in(false);
                        }
                        match res {
                            Ok(()) => {
                                this.show_toast("Sesión iniciada", "Conexión con Google Drive establecida exitosamente.", false);
                                this.refresh_periodic();
                                this.refresh_selective_folders();
                            }
                            Err(e) => {
                                this.show_error("Fallo de Autenticación", &e.to_string());
                            }
                        }
                    }).ok();
                });
            });
        }

        // Settings Actions
        {
            let this = self.clone();
            app.on_browse_mount_point(move || {
                if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                    if let Some(w) = this.app.upgrade() {
                        w.set_mount_point(dir.to_string_lossy().to_string().into());
                    }
                }
            });
        }
        {
            let this = self.clone();
            app.on_save_settings(move || {
                let Some(w) = this.app.upgrade() else { return };
                let mut cfg = this.cfg.lock().unwrap();
                cfg.mount_point = PathBuf::from(w.get_mount_point().as_str());
                if let Ok(gb) = w.get_cache_size_gb().as_str().parse::<u64>() {
                    cfg.cache_max_gb = gb;
                }
                if let Ok(poll) = w.get_poll_interval_secs().as_str().parse::<u64>() {
                    cfg.poll_interval_secs = poll;
                }
                cfg.notifications_enabled = w.get_notify_enabled();
                cfg.persistent_upload_notify = w.get_resident_notify();
                cfg.close_to_tray = w.get_close_to_tray();
                let _ = cfg.save();

                desktop::set_autostart_gui(w.get_autostart_gui());
                desktop::set_autostart_daemon(w.get_autostart_daemon());
                desktop::ensure_dolphin_bookmark(&cfg.mount_point);

                this.show_toast("Ajustes Guardados", "Los cambios se aplicaron correctamente.", false);
            });
        }
        {
            let app_weak = self.app.clone();
            app.on_request_resync(move || {
                if let Some(w) = app_weak.upgrade() {
                    w.set_confirm_title("¿Restablecer base de datos y caché?".into());
                    w.set_confirm_message("Se detendrá el servicio, se borrarán todos los archivos temporales en caché y el índice SQLite. Google Drive volverá a indexarse desde cero.".into());
                    w.set_confirm_btn_text("Restablecer (Resync)".into());
                    w.set_confirm_is_danger(true);
                    w.set_confirm_action_tag("resync".into());
                    w.set_confirm_modal_open(true);
                }
            });
        }

        // Logs Actions
        {
            let this = self.clone();
            app.on_refresh_logs(move || {
                this.refresh_logs();
                this.show_toast("Registros", "Salida de journalctl actualizada.", false);
            });
        }
        {
            let app_weak = self.app.clone();
            app.on_clear_logs(move || {
                if let Some(w) = app_weak.upgrade() {
                    w.set_logs(ModelRc::from(Rc::new(VecModel::default())));
                }
            });
        }

        // Modal Confirmation Handling
        {
            let this = self.clone();
            app.on_modal_confirmed(move |tag| {
                let tag_str = tag.to_string();
                if tag_str == "resync" {
                    let mount = this.cfg.lock().unwrap().mount_point.clone();
                    match workers::perform_resync(&mount) {
                        Ok(()) => {
                            this.show_toast("Resync completado", "Base de datos y caché restablecidas.", false);
                            this.refresh_periodic();
                            this.refresh_explorer();
                        }
                        Err(e) => {
                            this.show_error("Error al restablecer", &e.to_string());
                        }
                    }
                } else if tag_str == "logout" {
                    let token_file = gdsync_core::auth::Token::path();
                    let _ = std::fs::remove_file(token_file);
                    this.show_toast("Sesión cerrada", "Tokens eliminados correctamente.", false);
                    this.refresh_periodic();
                }
            });
        }
        app.on_modal_cancelled(|| {});

        // Pinned Modal Search & Unpin
        {
            let this = self.clone();
            app.on_pinned_modal_search(move |q| {
                *this.pinned_search_query.lock().unwrap() = q.to_string();
                this.update_pinned_view_models();
            });
        }
        {
            let this = self.clone();
            app.on_unpin_item(move |id| {
                let id_str = id.to_string();
                let _ = this.db.set_pinned_recursive(&id_str, false);
                this.show_toast("Liberado", "Elemento desmarcado de sin conexión.", false);
                this.refresh_pinned_items();
                this.refresh_explorer();
                this.refresh_periodic();
            });
        }

        // Console Link
        app.on_open_console_link(|| {
            desktop::open_web_url("https://console.cloud.google.com/");
        });
    }

    pub fn refresh_periodic(&self) {
        let Some(app) = self.app.upgrade() else { return };

        let daemon_running = desktop::is_daemon_running();
        app.set_daemon_running(daemon_running);

        let is_logged = gdsync_core::auth::Token::path().exists();
        app.set_account_connected(is_logged);

        let user_email = self.db.kv_get("account_email").unwrap_or(None).unwrap_or_default();
        if !user_email.is_empty() {
            let avatar = user_email.chars().next().unwrap_or('G').to_uppercase().to_string();
            app.set_user_email(user_email.clone().into());
            app.set_user_avatar(avatar.into());
            app.set_account_label(user_email.into());
        } else if is_logged {
            app.set_user_avatar("G".into());
            app.set_account_label("Sesión Activa".into());
        } else {
            app.set_user_avatar("?".into());
            app.set_account_label("No conectado".into());
        }

        // Storage and Cache Stats
        let cache_bytes = self.db.cache_total().unwrap_or(0);
        let cache_max_gb = self.cfg.lock().unwrap().cache_max_gb;
        let cache_limit_bytes = cache_max_gb * 1024 * 1024 * 1024;
        let cache_pct = if cache_limit_bytes > 0 {
            (cache_bytes as f32) / (cache_limit_bytes as f32)
        } else {
            0.0
        };

        app.set_cache_used_str(format_bytes(cache_bytes).into());
        app.set_cache_limit_str(format!("{} GB", cache_max_gb).into());
        app.set_cache_pct(cache_pct);

        let pinned_count = self.db.count_pinned_items().unwrap_or(0);
        app.set_pinned_count(pinned_count as i32);

        // Upload Queue Stats
        if let Ok(summary) = self.db.pending_uploads_summary() {
            let total_pending = summary.total;
            app.set_pending_uploads_count(total_pending as i32);
            app.set_pending_bytes_str(format_bytes(summary.upload_bytes).into());

            // Notifications
            let (notif_on, persist_on) = {
                let cfg = self.cfg.lock().unwrap();
                (cfg.notifications_enabled, cfg.persistent_upload_notify)
            };
            self.notifications.lock().unwrap().update_queue_status(
                total_pending as usize,
                summary.upload_bytes,
                notif_on,
                persist_on,
            );

            // Tray Status Text
            let tray_txt = if !daemon_running {
                "⚪ Servicio detenido".to_string()
            } else if total_pending > 0 {
                format!("📤 Subiendo {} archivo(s)...", total_pending)
            } else {
                "✅ Todo sincronizado".to_string()
            };
            *self.tray_status.lock().unwrap() = tray_txt;
        }

        // Queue Items Table
        if let Ok(q_items) = self.db.list_queue_items(100) {
            let entries: Vec<QueueEntry> = q_items
                .into_iter()
                .map(|q| QueueEntry {
                    id: q.id as i32,
                    op: q.op.into(),
                    file_id: q.file_id.into(),
                    display_name: q.display_name.into(),
                    size_str: format_bytes(q.size_bytes).into(),
                    status: q.status.clone().into(),
                    is_failed: q.status == "failed",
                    attempts: q.attempts as i32,
                    created_str: "".into(),
                    last_error: q.last_error.unwrap_or_default().into(),
                })
                .collect();
            app.set_queue_items(ModelRc::from(Rc::new(VecModel::from(entries))));
        }

        // Fetch storage quota periodically in background if logged in (throttled to once every 60s)
        if is_logged {
            let should_fetch = {
                let mut last = self.last_quota_fetch.lock().unwrap();
                match *last {
                    Some(instant) if instant.elapsed() < Duration::from_secs(60) => false,
                    _ => {
                        *last = Some(std::time::Instant::now());
                        true
                    }
                }
            };

            if should_fetch {
                if let Some(drive) = self.get_drive() {
                    let app_weak = self.app.clone();
                    let notif = self.notifications.clone();
                    let notif_enabled = self.cfg.lock().unwrap().notifications_enabled;

                    tokio::spawn(async move {
                        if let Ok((used, total)) = drive.about_storage().await {
                            let pct = if total > 0 { (used as f32) / (total as f32) } else { 0.0 };
                            let used_str = format_bytes(used);
                            let total_str = format_bytes(total);

                            notif.lock().unwrap().check_quota_warning(used, total, notif_enabled);

                            slint::invoke_from_event_loop(move || {
                                if let Some(w) = app_weak.upgrade() {
                                    w.set_drive_used_str(used_str.into());
                                    w.set_drive_total_str(total_str.into());
                                    w.set_drive_pct(pct);
                                }
                            }).ok();
                        }
                    });
                }
            }
        }
    }

    pub fn refresh_explorer(&self) {
        if let Ok(items) = self.db.list_explorer_items(1000) {
            *self.raw_explorer_items.lock().unwrap() = items;
            self.update_explorer_view_models();
        }
    }

    fn update_explorer_view_models(&self) {
        let Some(app) = self.app.upgrade() else { return };
        let items = self.raw_explorer_items.lock().unwrap().clone();
        let query = self.active_search_query.lock().unwrap().to_lowercase();
        let filter = *self.active_filter_idx.lock().unwrap();

        let entries: Vec<FileEntry> = items
            .into_iter()
            .filter(|item| {
                let matches_query = query.is_empty()
                    || item.name.to_lowercase().contains(&query)
                    || item.parent_location.to_lowercase().contains(&query);

                let matches_filter = match filter {
                    1 => item.status == LocalFileStatus::Pinned,
                    2 => item.status == LocalFileStatus::Cached,
                    3 => item.status == LocalFileStatus::CloudOnly,
                    _ => true,
                };

                matches_query && matches_filter
            })
            .map(|item| {
                let (status_code, status_text) = match item.status {
                    LocalFileStatus::Folder => (0, "Carpeta"),
                    LocalFileStatus::Pinned => (1, "Offline (Anclado)"),
                    LocalFileStatus::Cached => (2, "En caché local"),
                    LocalFileStatus::CloudOnly => (3, "Solo en la nube"),
                };

                FileEntry {
                    id: item.id.into(),
                    name: item.name.into(),
                    location: item.parent_location.into(),
                    size_str: format_bytes(item.size).into(),
                    mime: item.mime.into(),
                    is_folder: item.is_folder,
                    status: status_code,
                    status_text: status_text.into(),
                    pinned: item.pinned,
                    web_link: item.web_link.into(),
                    visible: true,
                }
            })
            .collect();

        app.set_files(ModelRc::from(Rc::new(VecModel::from(entries))));
    }

    pub fn refresh_pinned_items(&self) {
        if let Ok(items) = self.db.get_root_pinned_items() {
            *self.raw_pinned_items.lock().unwrap() = items;
            self.update_pinned_view_models();
        }
    }

    fn update_pinned_view_models(&self) {
        let Some(app) = self.app.upgrade() else { return };
        let items = self.raw_pinned_items.lock().unwrap().clone();
        let query = self.pinned_search_query.lock().unwrap().to_lowercase();

        let mut total_count = 0;
        let mut total_size = 0u64;

        let entries: Vec<PinnedItemEntry> = items
            .into_iter()
            .map(|p| {
                total_count += p.file_count;
                total_size += p.total_size;

                let matches_query = query.is_empty()
                    || p.name.to_lowercase().contains(&query)
                    || p.full_path.to_lowercase().contains(&query);

                let is_full = p.cached_count >= p.file_count;
                let cache_str = if is_full {
                    "Completo".to_string()
                } else {
                    format!("{}/{} archivos", p.cached_count, p.file_count)
                };

                PinnedItemEntry {
                    id: p.id.into(),
                    name: p.name.into(),
                    full_path: p.full_path.into(),
                    is_folder: p.is_folder,
                    count_str: format!("{} arch.", p.file_count).into(),
                    size_str: format_bytes(p.total_size).into(),
                    cache_str: cache_str.into(),
                    is_fully_cached: is_full,
                    visible: matches_query,
                }
            })
            .collect();

        let summary = format!(
            "Total: {} elementos | {} en disco",
            total_count,
            format_bytes(total_size)
        );
        app.set_pinned_summary(summary.into());
        app.set_pinned_items(ModelRc::from(Rc::new(VecModel::from(entries))));
    }

    pub fn refresh_selective_folders(&self) {
        let sync_folders = self.cfg.lock().unwrap().sync_folders.clone();
        let is_all = sync_folders.is_empty() || sync_folders.iter().any(|f| f == "root");

        // 1. Immediately populate from local SQLite database if items exist
        if let Some(app) = self.app.upgrade() {
            let mut nodes: Vec<FolderNode> = Vec::new();
            nodes.push(FolderNode {
                id: "root".into(),
                name: "Mi unidad (root)".into(),
                checked: is_all,
            });

            if let Ok(local_folders) = self.db.list_root_folders() {
                for (id, name) in local_folders {
                    let checked = is_all || sync_folders.contains(&id);
                    nodes.push(FolderNode {
                        id: id.into(),
                        name: name.into(),
                        checked,
                    });
                }
            }

            app.set_folder_nodes(ModelRc::from(Rc::new(VecModel::from(nodes))));
        }

        // 2. Fetch/update from Google Drive API asynchronously
        if let Some(drive) = self.get_drive() {
            let app_weak = self.app.clone();
            let cfg = self.cfg.clone();

            tokio::spawn(async move {
                if let Ok(folders) = workers::fetch_remote_folders(&drive).await {
                    let sync_folders = cfg.lock().unwrap().sync_folders.clone();
                    let is_all = sync_folders.is_empty() || sync_folders.iter().any(|f| f == "root");

                    let mut nodes: Vec<FolderNode> = Vec::new();
                    nodes.push(FolderNode {
                        id: "root".into(),
                        name: "Mi unidad (root)".into(),
                        checked: is_all,
                    });

                    for (id, name) in folders {
                        let checked = is_all || sync_folders.contains(&id);
                        nodes.push(FolderNode {
                            id: id.into(),
                            name: name.into(),
                            checked,
                        });
                    }

                    slint::invoke_from_event_loop(move || {
                        if let Some(w) = app_weak.upgrade() {
                            w.set_folder_nodes(ModelRc::from(Rc::new(VecModel::from(nodes))));
                        }
                    }).ok();
                }
            });
        }
    }

    pub fn refresh_logs(&self) {
        let Some(app) = self.app.upgrade() else { return };
        let parsed = workers::fetch_journal_logs(150);

        let entries: Vec<LogEntry> = parsed
            .into_iter()
            .map(|p| {
                let level_color = match p.level_color_code {
                    1 => Color::from_rgb_u8(16, 185, 129),   // success
                    2 => Color::from_rgb_u8(245, 158, 11),   // warning
                    3 => Color::from_rgb_u8(239, 68, 68),    // danger
                    _ => Color::from_rgb_u8(59, 130, 246),    // info
                };
                LogEntry {
                    timestamp: p.timestamp.into(),
                    level: p.level.into(),
                    level_color,
                    message: p.message.into(),
                }
            })
            .collect();

        app.set_logs(ModelRc::from(Rc::new(VecModel::from(entries))));
    }

    fn show_toast(&self, title: &str, message: &str, is_warning: bool) {
        if let Some(w) = self.app.upgrade() {
            w.set_toast_title(title.into());
            w.set_toast_message(message.into());
            w.set_toast_is_warning(is_warning);
            w.set_toast_visible(true);
        }
    }

    fn show_error(&self, title: &str, error_text: &str) {
        if let Some(w) = self.app.upgrade() {
            w.set_error_title(title.into());
            w.set_error_text(error_text.into());
            w.set_error_modal_open(true);
        }
    }
}
