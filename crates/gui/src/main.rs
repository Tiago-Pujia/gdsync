slint::include_modules!();

mod controller;
mod desktop;
mod ipc;
mod notifications;
mod tray;
mod workers;

use std::sync::{Arc, Mutex};
use anyhow::Result;
use gdsync_core::config::{self, Config};
use gdsync_core::db::Db;
use gdsync_core::drive::Drive;
use ksni::TrayMethods;
use slint::ComponentHandle;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let minimized = args.iter().any(|a| a == "--minimized" || a == "-m");
    let target_tab = args.windows(2).find_map(|w| {
        if w[0] == "--tab" || w[0] == "-t" {
            w[1].parse::<i32>().ok()
        } else {
            None
        }
    });

    let share_target = args.windows(2).find_map(|w| {
        if w[0] == "--share" || w[0] == "-s" {
            Some(w[1].clone())
        } else {
            None
        }
    });

    // 1. Single Instance Check via Unix Domain Socket
    let listener = match ipc::check_single_instance(minimized, target_tab, share_target.as_deref())? {
        ipc::InstanceCheck::AlreadyRunning => {
            println!("gdsync-gui ya se encuentra en ejecución. Trayendo ventana al frente.");
            return Ok(());
        }
        ipc::InstanceCheck::Primary(listener) => listener,
    };

    // 2. Initialize Tokio Runtime for background async tasks
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    let _guard = rt.enter();

    // 3. Load Config and Database
    let cfg = Arc::new(Mutex::new(Config::load_or_default()));
    let db = Arc::new(Db::open(&config::db_path())?);
    let drive: Arc<Mutex<Option<Arc<Drive>>>> = Arc::new(Mutex::new(
        Drive::new(cfg.lock().unwrap().clone()).ok().map(Arc::new),
    ));
    let notifications = Arc::new(Mutex::new(notifications::NotificationManager::new()));
    let tray_status = Arc::new(Mutex::new("✅ Todo sincronizado".to_string()));

    // 4. Create App Window
    let app = AppWindow::new()?;
    if let Some(tab) = target_tab {
        app.set_active_tab(tab);
    }
    let app_weak = app.as_weak();

    // 5. Initialize Controller and Slint bindings
    let controller = Arc::new(controller::GuiController::new(
        app_weak.clone(),
        db.clone(),
        cfg.clone(),
        drive.clone(),
        notifications.clone(),
        tray_status.clone(),
    ));
    controller.clone().init_and_bind();

    // 6. Spawn IPC Listener
    ipc::spawn_ipc_listener(listener, app_weak.clone(), controller.clone());

    // 6. Spawn System Tray via ksni
    let tray_inst = tray::GdsyncTray {
        status_text: tray_status.clone(),
        show_window_cb: {
            let app_weak = app_weak.clone();
            Arc::new(move || {
                let weak = app_weak.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(w) = weak.upgrade() {
                        w.window().show().ok();
                    }
                }).ok();
            })
        },
        open_dolphin_cb: {
            let cfg = cfg.clone();
            Arc::new(move || {
                let mount = cfg.lock().unwrap().mount_point.clone();
                desktop::open_path_in_file_manager(&mount);
            })
        },
        toggle_daemon_cb: {
            let cfg = cfg.clone();
            Arc::new(move || {
                let mount = cfg.lock().unwrap().mount_point.clone();
                if desktop::is_daemon_running() {
                    desktop::stop_daemon(&mount);
                } else {
                    desktop::start_daemon();
                }
            })
        },
        quit_cb: {
            let notif = notifications.clone();
            Arc::new(move || {
                notif.lock().unwrap().close_queue_notification();
                let _ = std::fs::remove_file(ipc::get_socket_path());
                slint::invoke_from_event_loop(|| {
                    slint::quit_event_loop().ok();
                }).ok();
            })
        },
    };

    let _tray_handle = rt.spawn(async move {
        let _ = tray_inst.spawn().await;
    });

    if let Some(ref target) = share_target {
        controller.open_share_modal(target);
    }

    // 8. Handle Window Close ("X" button)
    {
        let cfg = cfg.clone();
        let notif = notifications.clone();
        app.window().on_close_requested(move || {
            let close_to_tray = cfg.lock().unwrap().close_to_tray;
            if close_to_tray {
                slint::CloseRequestResponse::HideWindow
            } else {
                notif.lock().unwrap().close_queue_notification();
                let _ = std::fs::remove_file(ipc::get_socket_path());
                slint::quit_event_loop().ok();
                slint::CloseRequestResponse::HideWindow
            }
        });
    }

    // 9. Show window if not starting minimized or if share modal was requested
    if !minimized || share_target.is_some() {
        app.show()?;
    }

    // 10. Run Slint event loop
    app.run()?;

    // Cleanup on exit
    notifications.lock().unwrap().close_queue_notification();
    let _ = std::fs::remove_file(ipc::get_socket_path());

    Ok(())
}
