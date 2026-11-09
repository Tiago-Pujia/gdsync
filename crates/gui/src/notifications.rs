use std::process::Command;

pub struct NotificationManager {
    queue_notification_id: Option<u32>,
    last_notified_pending: Option<(usize, usize, u64)>,
    quota_warning_shown: bool,
}

impl NotificationManager {
    pub fn new() -> Self {
        Self {
            queue_notification_id: None,
            last_notified_pending: None,
            quota_warning_shown: false,
        }
    }

    pub fn notify(&self, title: &str, message: &str, is_warning: bool, enabled: bool) {
        if !enabled {
            return;
        }
        let urgency = if is_warning { "critical" } else { "normal" };
        let _ = Command::new("notify-send")
            .args([
                "-a", "gdsync",
                "-u", urgency,
                "-i", "folder-gdrive",
                "-h", "string:desktop-entry:gdsync",
                title,
                message,
            ])
            .spawn();
    }

    pub fn update_queue_status(
        &mut self,
        total: usize,
        upload_bytes: u64,
        enabled: bool,
        persistent: bool,
    ) {
        if !enabled {
            if self.queue_notification_id.is_some() {
                self.close_queue_notification();
            }
            self.last_notified_pending = None;
            return;
        }

        let current_key = (total, 0, upload_bytes);

        if total > 0 {
            if !persistent {
                if self.queue_notification_id.is_some() {
                    self.close_queue_notification();
                }
                self.last_notified_pending = Some(current_key);
                return;
            }

            if self.queue_notification_id.is_some() && self.last_notified_pending == Some(current_key) {
                return;
            }

            let status_text = format!(
                "Subiendo {} archivo(s): {}",
                total,
                format_bytes(upload_bytes)
            );

            let mut cmd = Command::new("notify-send");
            cmd.args([
                "-p",
                "-t", "0",
                "-h", "boolean:resident:true",
                "-h", "string:desktop-entry:gdsync",
                "-u", "low",
                "-a", "gdsync",
                "-i", "folder-gdrive",
            ]);

            if let Some(id) = self.queue_notification_id {
                cmd.args(["-r", &id.to_string()]);
            }

            cmd.args(["gdsync", &status_text]);

            if let Ok(output) = cmd.output() {
                if output.status.success() {
                    let out_str = String::from_utf8_lossy(&output.stdout).trim().to_string();
                    if let Ok(id) = out_str.parse::<u32>() {
                        self.queue_notification_id = Some(id);
                    }
                }
            }
            self.last_notified_pending = Some(current_key);
        } else {
            // total == 0
            if self.queue_notification_id.is_some()
                || (self.last_notified_pending.is_some() && self.last_notified_pending != Some((0, 0, 0)))
            {
                let mut cmd = Command::new("notify-send");
                cmd.args([
                    "-t", "5000",
                    "-u", "normal",
                    "-h", "string:desktop-entry:gdsync",
                    "-a", "gdsync",
                    "-i", "folder-gdrive",
                ]);
                if let Some(id) = self.queue_notification_id {
                    cmd.args(["-r", &id.to_string()]);
                }
                cmd.args([
                    "gdsync",
                    "Todas las tareas pendientes fueron subidas a Google Drive con éxito.",
                ]);
                let _ = cmd.output();
                self.queue_notification_id = None;
                self.last_notified_pending = Some((0, 0, 0));
            }
        }
    }

    pub fn check_quota_warning(&mut self, used_bytes: u64, total_bytes: u64, enabled: bool) {
        if !enabled || total_bytes == 0 {
            return;
        }
        let pct = (used_bytes as f64) / (total_bytes as f64);
        if pct >= 0.90 && !self.quota_warning_shown {
            self.quota_warning_shown = true;
            let used_str = format_bytes(used_bytes);
            let total_str = format_bytes(total_bytes);
            self.notify(
                "Espacio en Google Drive casi lleno",
                &format!(
                    "Estás utilizando el {:.0}% de tu cuota de Google Drive ({} de {}).",
                    pct * 100.0,
                    used_str,
                    total_str
                ),
                true,
                true,
            );
        } else if pct < 0.85 {
            self.quota_warning_shown = false;
        }
    }

    pub fn close_queue_notification(&mut self) {
        if let Some(id) = self.queue_notification_id.take() {
            let _ = Command::new("gdbus")
                .args([
                    "call",
                    "--session",
                    "--dest", "org.freedesktop.Notifications",
                    "--object-path", "/org/freedesktop/Notifications",
                    "--method", "org.freedesktop.Notifications.CloseNotification",
                    &id.to_string(),
                ])
                .output();
        }
    }
}

pub fn format_bytes(bytes: u64) -> String {
    let mut n = bytes as f64;
    for unit in ["B", "KB", "MB", "GB", "TB"] {
        if n < 1024.0 {
            return format!("{:.1} {}", n, unit);
        }
        n /= 1024.0;
    }
    format!("{:.1} PB", n)
}
