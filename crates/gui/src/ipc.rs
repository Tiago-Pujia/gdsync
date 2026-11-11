use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;
use anyhow::{Context, Result};
use slint::ComponentHandle;
use crate::controller::GuiController;

pub fn get_socket_path() -> PathBuf {
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR") {
        PathBuf::from(runtime_dir).join("gdsync_gui.sock")
    } else {
        let uid = unsafe { libc::getuid() };
        PathBuf::from(format!("/tmp/gdsync_gui_{}.sock", uid))
    }
}

pub enum InstanceCheck {
    Primary(UnixListener),
    AlreadyRunning,
}

pub fn check_single_instance(
    minimized: bool,
    target_tab: Option<i32>,
    share_target: Option<&str>,
) -> Result<InstanceCheck> {
    let socket_path = get_socket_path();

    if socket_path.exists() {
        if let Ok(mut stream) = UnixStream::connect(&socket_path) {
            if let Some(target) = share_target {
                let msg = format!("SHARE {}\n", target);
                let _ = stream.write_all(msg.as_bytes());
                let _ = stream.flush();
            } else if let Some(tab) = target_tab {
                let msg = format!("TAB {}\n", tab);
                let _ = stream.write_all(msg.as_bytes());
                let _ = stream.flush();
            } else if !minimized {
                let _ = stream.write_all(b"SHOW\n");
                let _ = stream.flush();
            }
            return Ok(InstanceCheck::AlreadyRunning);
        } else {
            // Stale socket from crashed instance
            let _ = fs::remove_file(&socket_path);
        }
    }

    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("No se pudo crear el socket IPC en {:?}", socket_path))?;
    listener.set_nonblocking(true)?;

    Ok(InstanceCheck::Primary(listener))
}

pub fn spawn_ipc_listener(
    std_listener: UnixListener,
    window_handle: slint::Weak<crate::AppWindow>,
    controller: Arc<GuiController>,
) {
    std::thread::spawn(move || {
        let _ = std_listener.set_nonblocking(false);
        for stream in std_listener.incoming() {
            if let Ok(stream) = stream {
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                if reader.read_line(&mut line).is_ok() {
                    let cmd = line.trim();
                    if cmd == "SHOW" {
                        let handle = window_handle.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = handle.upgrade() {
                                w.window().show().ok();
                            }
                        });
                    } else if let Some(target) = cmd.strip_prefix("SHARE ") {
                        let c = controller.clone();
                        let target_str = target.to_string();
                        let _ = slint::invoke_from_event_loop(move || {
                            c.open_share_modal(&target_str);
                        });
                    }
 else if let Some(tab_str) = cmd.strip_prefix("TAB ") {
                        if let Ok(idx) = tab_str.parse::<i32>() {
                            let handle = window_handle.clone();
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(w) = handle.upgrade() {
                                    w.set_active_tab(idx);
                                    w.window().show().ok();
                                }
                            });
                        }
                    } else if cmd == "MODAL PINNED" {
                        let handle = window_handle.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = handle.upgrade() {
                                w.set_pinned_modal_open(true);
                                w.window().show().ok();
                            }
                        });
                    } else if cmd == "MODAL TUTORIAL" {
                        let handle = window_handle.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = handle.upgrade() {
                                w.set_tutorial_modal_open(true);
                                w.set_tutorial_step(0);
                                w.set_tutorial_view_all(false);
                                w.window().show().ok();
                            }
                        });
                    } else if let Some(step_str) = cmd.strip_prefix("MODAL TUTORIAL STEP ") {
                        if let Ok(st) = step_str.parse::<i32>() {
                            let handle = window_handle.clone();
                            let _ = slint::invoke_from_event_loop(move || {
                                if let Some(w) = handle.upgrade() {
                                    w.set_tutorial_modal_open(true);
                                    w.set_tutorial_step(st);
                                    w.set_tutorial_view_all(false);
                                    w.window().show().ok();
                                }
                            });
                        }
                    } else if cmd == "MODAL TUTORIAL ALL" {
                        let handle = window_handle.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = handle.upgrade() {
                                w.set_tutorial_modal_open(true);
                                w.set_tutorial_view_all(true);
                                w.window().show().ok();
                            }
                        });
                    } else if cmd == "CLOSE MODALS" {
                        let handle = window_handle.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(w) = handle.upgrade() {
                                w.set_pinned_modal_open(false);
                                w.set_tutorial_modal_open(false);
                                w.set_confirm_modal_open(false);
                                w.set_error_modal_open(false);
                                w.set_share_modal_open(false);
                            }
                        });
                    }
                }
            }
        }
    });
}
