use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub fn is_daemon_running() -> bool {
    // Check systemd unit first
    if let Ok(output) = Command::new("systemctl")
        .args(["--user", "is-active", "gdsyncd"])
        .output()
    {
        if String::from_utf8_lossy(&output.stdout).trim() == "active" {
            return true;
        }
    }

    // Fallback to pgrep
    if let Ok(output) = Command::new("pgrep").args(["-f", "gdsyncd"]).output() {
        return !output.stdout.is_empty();
    }

    false
}

pub fn start_daemon() -> bool {
    let status = Command::new("systemctl")
        .args(["--user", "start", "gdsyncd"])
        .status();
    match status {
        Ok(s) if s.success() => true,
        _ => {
            // Fallback: spawn gdsyncd binary directly if systemd unit is not loaded
            let candidates = [
                dirs::home_dir().map(|h| h.join(".local/bin/gdsyncd")),
                Some(PathBuf::from("/usr/local/bin/gdsyncd")),
                Some(PathBuf::from("/usr/bin/gdsyncd")),
            ];
            for c in candidates.into_iter().flatten() {
                if c.exists() {
                    return Command::new(c).spawn().is_ok();
                }
            }
            Command::new("gdsyncd").spawn().is_ok()
        }
    }
}

pub fn stop_daemon(mount_point: &Path) -> bool {
    let _ = Command::new("systemctl")
        .args(["--user", "stop", "gdsyncd"])
        .status();

    // Ensure FUSE mount is cleanly unmounted
    let _ = Command::new("fusermount3")
        .args(["-u", &mount_point.to_string_lossy()])
        .output();

    !is_daemon_running()
}

pub fn is_autostart_gui() -> bool {
    if let Some(home) = dirs::home_dir() {
        home.join(".config/autostart/gdsync-gui.desktop").exists()
    } else {
        false
    }
}

pub fn set_autostart_gui(enable: bool) {
    if let Some(home) = dirs::home_dir() {
        let autostart_dir = home.join(".config/autostart");
        let desktop_file = autostart_dir.join("gdsync-gui.desktop");

        if enable {
            let _ = fs::create_dir_all(&autostart_dir);
            let content = "[Desktop Entry]\n\
Type=Application\n\
Name=gdsync\n\
Comment=Google Drive FUSE Client\n\
Exec=gdsync-gui --minimized\n\
Icon=folder-gdrive\n\
Terminal=false\n\
Categories=Utility;Network;\n\
X-GNOME-Autostart-enabled=true\n";
            let _ = fs::write(desktop_file, content);
        } else if desktop_file.exists() {
            let _ = fs::remove_file(desktop_file);
        }
    }
}

pub fn is_autostart_daemon() -> bool {
    if let Ok(output) = Command::new("systemctl")
        .args(["--user", "is-enabled", "gdsyncd"])
        .output()
    {
        String::from_utf8_lossy(&output.stdout).trim() == "enabled"
    } else {
        false
    }
}

pub fn set_autostart_daemon(enable: bool) {
    let action = if enable { "enable" } else { "disable" };
    let _ = Command::new("systemctl")
        .args(["--user", action, "gdsyncd"])
        .status();
}

pub fn ensure_dolphin_bookmark(mount_point: &Path) {
    if let Some(home) = dirs::home_dir() {
        let places_file = home.join(".local/share/user-places.xbel");
        if !places_file.exists() {
            return;
        }

        if let Ok(content) = fs::read_to_string(&places_file) {
            let resolved = mount_point.canonicalize().unwrap_or_else(|_| mount_point.to_path_buf());
            let target_uri = format!("file://{}", resolved.to_string_lossy());

            if content.contains(&target_uri) || content.contains("gdsync-googledrive") {
                return;
            }

            let bookmark_xml = format!(
                "  <bookmark href=\"{}\">\n\
   <title>Google Drive</title>\n\
   <info>\n\
    <metadata owner=\"http://freedesktop.org\">\n\
     <bookmark:icon name=\"folder-gdrive\"/>\n\
    </metadata>\n\
    <metadata owner=\"http://www.kde.org\">\n\
     <ID>gdsync-googledrive</ID>\n\
     <isSystemItem>false</isSystemItem>\n\
    </metadata>\n\
   </info>\n\
  </bookmark>\n</xbel>",
                target_uri
            );

            if content.contains("</xbel>") {
                let updated = content.replace("</xbel>", &bookmark_xml);
                let _ = fs::write(&places_file, updated);
            }
        }
    }
}

pub fn open_path_in_file_manager(path: &Path) {
    let _ = Command::new("xdg-open")
        .arg(path)
        .spawn();
}

pub fn open_web_url(url: &str) {
    let _ = Command::new("xdg-open")
        .arg(url)
        .spawn();
}
