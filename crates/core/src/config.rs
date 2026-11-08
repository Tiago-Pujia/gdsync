use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

const APP: &str = "gdsync";

pub fn config_dir() -> PathBuf {
    dirs::config_dir().expect("sin directorio de configuración").join(APP)
}
pub fn data_dir() -> PathBuf {
    dirs::data_dir().expect("sin directorio de datos").join(APP)
}
pub fn cache_dir() -> PathBuf {
    dirs::cache_dir().expect("sin directorio de caché").join(APP)
}
pub fn db_path() -> PathBuf {
    data_dir().join("state.db")
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Config {
    pub client_id: String,
    pub client_secret: String,
    #[serde(default = "default_mount")]
    pub mount_point: PathBuf,
    #[serde(default = "default_poll")]
    pub poll_interval_secs: u64,
    /// Max size of the content cache (pinned files are never evicted).
    #[serde(default = "default_cache_gb")]
    pub cache_max_gb: u64,
    /// Folders selected for sync (defaults to ["root"] for full drive).
    #[serde(default = "default_sync_folders")]
    pub sync_folders: Vec<String>,
    /// Auto-retry failed uploads interval in seconds (0 = disabled, default 300 = 5 minutes).
    #[serde(default = "default_auto_retry_secs")]
    pub auto_retry_secs: u64,
    /// Whether desktop notifications are enabled.
    #[serde(default = "default_notifications_enabled")]
    pub notifications_enabled: bool,
    /// Whether persistent desktop notification for pending uploads is enabled.
    #[serde(default = "default_persistent_upload_notify")]
    pub persistent_upload_notify: bool,
    /// Whether to minimize to tray on close.
    #[serde(default = "default_close_to_tray")]
    pub close_to_tray: bool,
}

fn default_mount() -> PathBuf {
    dirs::home_dir().expect("sin HOME").join("GoogleDrive")
}
fn default_poll() -> u64 {
    30
}
fn default_cache_gb() -> u64 {
    10
}
fn default_sync_folders() -> Vec<String> {
    vec!["root".into()]
}
fn default_auto_retry_secs() -> u64 {
    300
}
fn default_notifications_enabled() -> bool {
    true
}
fn default_persistent_upload_notify() -> bool {
    true
}
fn default_close_to_tray() -> bool {
    false
}

impl Config {
    pub fn new(client_id: String, client_secret: String) -> Self {
        Self {
            client_id,
            client_secret,
            mount_point: default_mount(),
            poll_interval_secs: default_poll(),
            cache_max_gb: default_cache_gb(),
            sync_folders: default_sync_folders(),
            auto_retry_secs: default_auto_retry_secs(),
            notifications_enabled: default_notifications_enabled(),
            persistent_upload_notify: default_persistent_upload_notify(),
            close_to_tray: default_close_to_tray(),
        }
    }
    pub fn path() -> PathBuf {
        config_dir().join("config.json")
    }
    pub fn load() -> Result<Self> {
        let s = fs::read_to_string(Self::path()).with_context(|| {
            "no hay configuración; ejecutá `gdsync login --client-id ... --client-secret ...`"
        })?;
        Ok(serde_json::from_str(&s)?)
    }
    pub fn load_or_default() -> Self {
        Self::load().unwrap_or_else(|_| Self::new(String::new(), String::new()))
    }
    pub fn save(&self) -> Result<()> {
        write_private(&Self::path(), &serde_json::to_vec_pretty(self)?)
    }

    pub fn is_syncing_all(&self) -> bool {
        self.sync_folders.is_empty() || self.sync_folders.iter().any(|f| f == "root")
    }

    pub fn add_sync_folder(&mut self, folder_id: String) -> bool {
        if self.sync_folders == vec!["root".to_string()] {
            self.sync_folders.clear();
        }
        if !self.sync_folders.contains(&folder_id) {
            self.sync_folders.push(folder_id);
            true
        } else {
            false
        }
    }

    pub fn remove_sync_folder(&mut self, folder_id: &str) -> bool {
        let prev_len = self.sync_folders.len();
        self.sync_folders.retain(|f| f != folder_id);
        let removed = self.sync_folders.len() != prev_len;
        if self.sync_folders.is_empty() {
            self.sync_folders.push("root".into());
        }
        removed
    }

    pub fn reset_sync_folders(&mut self) {
        self.sync_folders = vec!["root".into()];
    }
}

/// Atomically writes a file readable only by the current user.
pub fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        f.write_all(bytes)?;
    }
    fs::rename(&tmp, path)?;
    Ok(())
}

/// Extracts (client_id, client_secret) from a Google Cloud Console credentials JSON string.
/// Handles standard format `{"installed": {"client_id": "...", "client_secret": "..."}}`,
/// `{"web": {...}}`, or flat `{"client_id": "...", "client_secret": "..."}`.
pub fn parse_google_credentials_json(content: &str) -> Result<(String, String)> {
    let val: serde_json::Value = serde_json::from_str(content)
        .context("el archivo no contiene un JSON válido")?;

    let node = if let Some(installed) = val.get("installed") {
        installed
    } else if let Some(web) = val.get("web") {
        web
    } else {
        &val
    };

    let client_id = node.get("client_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .context("no se encontró 'client_id' en el archivo JSON de Google")?
        .trim()
        .to_string();

    let client_secret = node.get("client_secret")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .context("no se encontró 'client_secret' en el archivo JSON de Google")?
        .trim()
        .to_string();

    Ok((client_id, client_secret))
}

/// Reads a credentials JSON file from disk and extracts (client_id, client_secret).
pub fn parse_google_credentials_file(path: &Path) -> Result<(String, String)> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("no se pudo leer el archivo de credenciales en {}", path.display()))?;
    parse_google_credentials_json(&content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sync_folder_selection() {
        let mut cfg = Config::new("test_id".into(), "test_secret".into());
        assert!(cfg.is_syncing_all());
        assert_eq!(cfg.sync_folders, vec!["root".to_string()]);

        // Adding folder 1 replaces root
        assert!(cfg.add_sync_folder("folder_a".into()));
        assert!(!cfg.is_syncing_all());
        assert_eq!(cfg.sync_folders, vec!["folder_a".to_string()]);

        // Adding second folder
        assert!(cfg.add_sync_folder("folder_b".into()));
        assert_eq!(cfg.sync_folders, vec!["folder_a".to_string(), "folder_b".to_string()]);

        // Adding existing folder returns false
        assert!(!cfg.add_sync_folder("folder_a".into()));

        // Removing folder_a
        assert!(cfg.remove_sync_folder("folder_a"));
        assert_eq!(cfg.sync_folders, vec!["folder_b".to_string()]);

        // Removing last folder restores root
        assert!(cfg.remove_sync_folder("folder_b"));
        assert!(cfg.is_syncing_all());
        assert_eq!(cfg.sync_folders, vec!["root".to_string()]);

        // Manual reset
        cfg.add_sync_folder("folder_c".into());
        assert!(!cfg.is_syncing_all());
        cfg.reset_sync_folders();
        assert!(cfg.is_syncing_all());
    }

    #[test]
    fn test_config_serialization_and_defaults() {
        let json = r#"{
            "client_id": "foo",
            "client_secret": "bar"
        }"#;
        let cfg: Config = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.client_id, "foo");
        assert_eq!(cfg.client_secret, "bar");
        assert_eq!(cfg.poll_interval_secs, 30);
        assert_eq!(cfg.cache_max_gb, 10);
        assert_eq!(cfg.sync_folders, vec!["root".to_string()]);
        assert!(cfg.is_syncing_all());
        assert!(cfg.notifications_enabled);
        assert!(cfg.persistent_upload_notify);
        assert!(!cfg.close_to_tray);

        let serialized = serde_json::to_string(&cfg).unwrap();
        let parsed: Config = serde_json::from_str(&serialized).unwrap();
        assert_eq!(parsed.client_id, "foo");
        assert_eq!(parsed.cache_max_gb, 10);
        assert!(parsed.notifications_enabled);
        assert!(parsed.persistent_upload_notify);
        assert!(!parsed.close_to_tray);

        let disabled_json = r#"{
            "client_id": "foo",
            "client_secret": "bar",
            "notifications_enabled": false,
            "persistent_upload_notify": false,
            "close_to_tray": true
        }"#;
        let disabled_cfg: Config = serde_json::from_str(disabled_json).unwrap();
        assert!(!disabled_cfg.notifications_enabled);
        assert!(!disabled_cfg.persistent_upload_notify);
        assert!(disabled_cfg.close_to_tray);
    }

    #[test]
    fn test_parse_google_credentials_json() {
        let installed_json = r#"{
            "installed": {
                "client_id": "test_id_installed.apps.googleusercontent.com",
                "project_id": "test_proj",
                "auth_uri": "https://accounts.google.com/o/oauth2/auth",
                "token_uri": "https://oauth2.googleapis.com/token",
                "client_secret": "secret_installed_123"
            }
        }"#;
        let (id, secret) = parse_google_credentials_json(installed_json).unwrap();
        assert_eq!(id, "test_id_installed.apps.googleusercontent.com");
        assert_eq!(secret, "secret_installed_123");

        let web_json = r#"{
            "web": {
                "client_id": "test_id_web.apps.googleusercontent.com",
                "client_secret": "secret_web_456"
            }
        }"#;
        let (id, secret) = parse_google_credentials_json(web_json).unwrap();
        assert_eq!(id, "test_id_web.apps.googleusercontent.com");
        assert_eq!(secret, "secret_web_456");

        let flat_json = r#"{
            "client_id": "test_id_flat",
            "client_secret": "secret_flat"
        }"#;
        let (id, secret) = parse_google_credentials_json(flat_json).unwrap();
        assert_eq!(id, "test_id_flat");
        assert_eq!(secret, "secret_flat");

        let invalid = r#"{"other": {}}"#;
        assert!(parse_google_credentials_json(invalid).is_err());
    }
}
