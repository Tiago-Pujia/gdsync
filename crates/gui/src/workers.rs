use std::fs;
use std::path::Path;
use std::process::Command;
use anyhow::{bail, Context, Result};
use gdsync_core::auth;
use gdsync_core::config::Config;
use gdsync_core::drive::Drive;

pub fn parse_google_credentials_json(path: &Path) -> Result<(String, String)> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("No se pudo leer el archivo {:?}", path))?;
    let val: serde_json::Value = serde_json::from_str(&content)
        .context("El archivo JSON no tiene un formato válido.")?;

    let node = val.get("installed")
        .or_else(|| val.get("web"))
        .unwrap_or(&val);

    let client_id = node.get("client_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();

    let client_secret = node.get("client_secret")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();

    if client_id.is_empty() || client_secret.is_empty() {
        bail!("El archivo no contiene los campos obligatorios 'client_id' y 'client_secret'.");
    }

    Ok((client_id, client_secret))
}

pub async fn run_oauth_login(client_id: &str, client_secret: &str) -> Result<()> {
    let mut cfg = Config::load_or_default();
    cfg.client_id = client_id.to_string();
    cfg.client_secret = client_secret.to_string();
    let _ = cfg.save();
    auth::login(&cfg).await?;
    Ok(())
}

pub async fn fetch_remote_folders(drive: &Drive) -> Result<Vec<(String, String)>> {
    let folders = drive.list_all_folders().await?;
    Ok(folders.into_iter().map(|f| (f.id, f.name)).collect())
}

pub struct ParsedLogLine {
    pub timestamp: String,
    pub level: String,
    pub level_color_code: u8, // 0: info, 1: ok/success, 2: warn, 3: err
    pub message: String,
}

pub fn fetch_journal_logs(line_count: usize) -> Vec<ParsedLogLine> {
    let output = Command::new("journalctl")
        .args([
            "--user",
            "-u", "gdsyncd",
            "-n", &line_count.to_string(),
            "--no-pager",
            "-o", "short-iso",
        ])
        .output();

    let mut result = Vec::new();
    if let Ok(out) = output {
        let stdout = String::from_utf8_lossy(&out.stdout);
        for line in stdout.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            let mut parts = line.splitn(4, ' ');
            let timestamp_raw = parts.next().unwrap_or("");
            let _host = parts.next();
            let _tag = parts.next();
            let msg_raw = parts.next().unwrap_or(line);

            let time_short = if timestamp_raw.len() >= 19 {
                &timestamp_raw[11..19]
            } else {
                timestamp_raw
            };

            let (level, level_color_code, msg) = if msg_raw.contains("[ERROR]") || msg_raw.contains("ERROR:") {
                ("[ERROR]", 3, msg_raw)
            } else if msg_raw.contains("[WARN]") || msg_raw.contains("WARN:") {
                ("[WARN]", 2, msg_raw)
            } else if msg_raw.contains("[OK]") || msg_raw.contains("SUCCESS") || msg_raw.contains("exitosamente") {
                ("[OK]", 1, msg_raw)
            } else {
                ("[INFO]", 0, msg_raw)
            };

            result.push(ParsedLogLine {
                timestamp: format!("[{}]", time_short),
                level: level.to_string(),
                level_color_code,
                message: msg.to_string(),
            });
        }
    }

    result
}

pub fn perform_resync(mount_point: &Path) -> Result<()> {
    // 1. Stop daemon
    crate::desktop::stop_daemon(mount_point);

    // 2. Remove state.db and cache
    let data_dir = gdsync_core::config::data_dir();
    let cache_dir = gdsync_core::config::cache_dir();

    let _ = fs::remove_file(data_dir.join("state.db"));
    let _ = fs::remove_file(data_dir.join("state.db-wal"));
    let _ = fs::remove_file(data_dir.join("state.db-shm"));

    let _ = fs::remove_dir_all(&cache_dir);
    let _ = fs::create_dir_all(&cache_dir);

    // 3. Restart daemon
    crate::desktop::start_daemon();

    Ok(())
}
