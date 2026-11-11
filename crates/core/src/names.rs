//! Maps Drive names to valid, unique local file names within a directory.
use crate::db::{Db, Item};
use std::collections::HashSet;
use std::path::Path;

/// Extension and icon for Google Docs types, which are shown as `.desktop` links.
fn google_kind(mime: &str) -> (&'static str, &'static str) {
    match mime.trim_start_matches(crate::drive::GOOGLE_APPS_PREFIX) {
        "document" => (".gdoc", "x-office-document"),
        "spreadsheet" => (".gsheet", "x-office-spreadsheet"),
        "presentation" => (".gslides", "x-office-presentation"),
        "drawing" => (".gdraw", "x-office-drawing"),
        "form" => (".gform", "text-html"),
        "jam" => (".gjam", "text-html"),
        "map" => (".gmap", "text-html"),
        "site" => (".gsite", "text-html"),
        "shortcut" => (".glink", "emblem-symbolic-link"),
        _ => (".glink", "text-html"),
    }
}

/// Linux forbids `/` and NUL in names, and `.`/`..` are reserved.
fn sanitize(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| match c {
            '/' => '∕', // U+2215 DIVISION SLASH
            '\0' => '_',
            c => c,
        })
        .collect();
    match s.as_str() {
        "" => "_".into(),
        "." | ".." => format!("{s}_"),
        _ => s,
    }
}

fn base_name(item: &Item) -> String {
    let name = sanitize(&item.name);
    if item.is_google_doc() {
        format!("{name}{}.desktop", google_kind(&item.mime).0)
    } else {
        name
    }
}

/// `foto.jpg` + 2 -> `foto (2).jpg`
fn with_suffix(name: &str, n: usize) -> String {
    match name.rfind('.') {
        Some(i) if i > 0 => format!("{} ({n}){}", &name[..i], &name[i..]),
        _ => format!("{name} ({n})"),
    }
}

/// Local entries of a directory. `children` must be ordered by (name, id), as
/// returned by `Db::children`, so suffixes are stable between calls.
pub fn local_entries(children: Vec<Item>) -> Vec<(String, Item)> {
    let bases: Vec<String> = children.iter().map(base_name).collect();

    // First occurrence of each name keeps it; later duplicates get " (n)".
    let mut taken: HashSet<String> = HashSet::new();
    let mut first = vec![false; children.len()];
    for (i, b) in bases.iter().enumerate() {
        first[i] = taken.insert(b.clone());
    }
    let mut out = Vec::with_capacity(children.len());
    for ((item, base), is_first) in children.into_iter().zip(bases).zip(first) {
        let name = if is_first {
            base
        } else {
            let mut n = 2;
            while taken.contains(&with_suffix(&base, n)) {
                n += 1;
            }
            let name = with_suffix(&base, n);
            taken.insert(name.clone());
            name
        };
        out.push((name, item));
    }
    out
}

/// Content of the `.desktop` link that represents a Google Doc.
pub fn desktop_link(item: &Item) -> String {
    let url = if item.is_shortcut() {
        match &item.web_link {
            Some(link) if link.contains("/drive/folders/") => link.clone(),
            Some(link) if link.contains("/file/d/") => format!("https://drive.google.com/open?id={}", item.id),
            Some(link) => link.clone(),
            None => format!("https://drive.google.com/open?id={}", item.id),
        }
    } else {
        item.web_link
            .clone()
            .unwrap_or_else(|| format!("https://drive.google.com/open?id={}", item.id))
    };
    let name = item.name.replace(['\n', '\r'], " ");
    let icon = if item.is_shortcut() && url.contains("/drive/folders/") {
        "folder-link"
    } else {
        google_kind(&item.mime).1
    };
    format!(
        "[Desktop Entry]\nType=Link\nName={name}\nURL={url}\nIcon={icon}\n"
    )
}

/// Content of the `.desktop` link that represents a Google Doc or a shortcut.
/// If `item` is a shortcut:
/// - If target is a Google Doc, opens the Google Docs web URL in the browser.
/// - If target is a regular file or folder present locally in FUSE, opens the local file:// URL in the file manager / app.
/// - If target is not present locally, falls back to opening Google Drive web URL in the browser.
pub fn desktop_link_resolved(item: &Item, db: &Db, mount_point: &Path) -> String {
    if !item.is_shortcut() {
        return desktop_link(item);
    }

    let target_id = item.shortcut_target_id();
    let target_item = target_id.as_deref().and_then(|tid| db.by_id(tid).ok().flatten());

    if let Some(target) = target_item {
        // 1. If target is a Google Doc, always open in browser
        if target.is_google_doc() && !target.is_shortcut() {
            let url = target
                .web_link
                .unwrap_or_else(|| format!("https://drive.google.com/open?id={}", target.id));
            let name = item.name.replace(['\n', '\r'], " ");
            let icon = google_kind(&target.mime).1;
            return format!("[Desktop Entry]\nType=Link\nName={name}\nURL={url}\nIcon={icon}\n");
        }

        // 2. If target is a normal file or folder, check if it exists locally in FUSE
        if let Ok(Some(rel_path)) = db.resolve_item_relative_path(&target.id) {
            let full_path = mount_point.join(rel_path);
            let url = format!("file://{}", full_path.to_string_lossy());
            let name = item.name.replace(['\n', '\r'], " ");
            let icon = if target.is_dir() {
                "folder-link"
            } else {
                "emblem-symbolic-link"
            };
            return format!("[Desktop Entry]\nType=Link\nName={name}\nURL={url}\nIcon={icon}\n");
        }
    }

    // Fallback: browser link to Google Drive web
    desktop_link(item)
}

/// True for temporary/lock/swap files created by editors and file managers
/// that should stay local in FUSE and never be uploaded as standalone files.
pub fn is_temp_file(name: &str) -> bool {
    name.starts_with(".goutputstream-")
        || name.starts_with(".~lock.")
        || name.starts_with(".#")
        || (name.starts_with('#') && name.ends_with('#') && name.len() > 1)
        || name.starts_with(".fuse_hidden")
        || name.starts_with(".Trash-")
        || name.ends_with('~')
        || name.ends_with(".swp")
        || name.ends_with(".swo")
        || name.ends_with(".swn")
        || name.ends_with(".swx")
        || name.ends_with(".kate-swp")
        || name.ends_with(".part")
        || name.ends_with(".crdownload")
        || name.ends_with(".tmp")
        || matches!(name, ".DS_Store" | "Thumbs.db" | "4913")
}

/// Decodes percent-encoded URL strings like %20 -> space.
pub fn decode_percent(s: &str) -> String {
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


#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, name: &str, mime: &str) -> Item {
        Item {
            ino: 0,
            id: id.into(),
            parent_id: None,
            name: name.into(),
            mime: mime.into(),
            size: 0,
            md5: None,
            mtime: 0,
            web_link: None,
            pinned: false,
        }
    }

    fn names(items: Vec<Item>) -> Vec<String> {
        local_entries(items).into_iter().map(|(n, _)| n).collect()
    }

    #[test]
    fn duplicates_get_suffix_and_avoid_collisions() {
        let got = names(vec![
            item("1", "foto.jpg", "image/jpeg"),
            item("2", "foto.jpg", "image/jpeg"),
            item("3", "foto (2).jpg", "image/jpeg"),
            item("4", "LEEME", "text/plain"),
            item("5", "LEEME", "text/plain"),
        ]);
        assert_eq!(got, vec!["foto.jpg", "foto (3).jpg", "foto (2).jpg", "LEEME", "LEEME (2)"]);
    }

    #[test]
    fn invalid_names_and_google_docs() {
        let got = names(vec![
            item("1", "a/b", "text/plain"),
            item("2", "..", "text/plain"),
            item("3", "Informe", "application/vnd.google-apps.document"),
            item("4", "atajo", "application/vnd.google-apps.shortcut"),
        ]);
        assert_eq!(got, vec!["a∕b", ".._", "Informe.gdoc.desktop", "atajo.glink.desktop"]);
    }

    #[test]
    fn desktop_link_points_to_drive() {
        let s = desktop_link(&item("XYZ", "Plan", "application/vnd.google-apps.spreadsheet"));
        assert!(s.contains("URL=https://drive.google.com/open?id=XYZ"));
        assert!(s.contains("Icon=x-office-spreadsheet"));

        let shortcut = desktop_link(&item("SC1", "Mi Atajo", "application/vnd.google-apps.shortcut"));
        assert!(shortcut.contains("URL=https://drive.google.com/open?id=SC1"));
        assert!(shortcut.contains("Icon=emblem-symbolic-link"));

        let mut sc_folder = item("SC2", "Carpeta Atajo", "application/vnd.google-apps.shortcut");
        sc_folder.web_link = Some("https://drive.google.com/drive/folders/TARGET_123".into());
        let s_folder = desktop_link(&sc_folder);
        assert!(s_folder.contains("URL=https://drive.google.com/drive/folders/TARGET_123"));
        assert!(s_folder.contains("Icon=folder-link"));

        let mut sc_broken = item("SC3", "Atajo Roto", "application/vnd.google-apps.shortcut");
        sc_broken.web_link = Some("https://drive.google.com/file/d/SC3/view?usp=drivesdk".into());
        let s_broken = desktop_link(&sc_broken);
        assert!(s_broken.contains("URL=https://drive.google.com/open?id=SC3"));
    }

    #[test]
    fn detects_editor_temp_files() {
        assert!(is_temp_file(".goutputstream-ABC123"));
        assert!(is_temp_file(".~lock.informe.odt#"));
        assert!(is_temp_file(".nota.txt.swp"));
        assert!(is_temp_file("nota.txt~"));
        assert!(is_temp_file("#autosave.txt#"));
        assert!(is_temp_file(".#lock.txt"));
        assert!(is_temp_file("descarga.part"));
        assert!(is_temp_file("4913"));
        assert!(!is_temp_file("nota.txt"));
        assert!(!is_temp_file(".gitignore"));
        assert!(!is_temp_file("informe.odt"));
    }

    #[test]
    fn test_desktop_link_resolved() {
        let db = Db::open_in_memory().unwrap();
        let rf = |id: &str, name: &str, parent: &str, mime: &str| crate::drive::RemoteFile {
            id: id.into(),
            name: name.into(),
            mime_type: mime.into(),
            size: Some("42".into()),
            md5_checksum: Some("abc".into()),
            modified_time: Some("2024-05-01T12:34:56.789Z".into()),
            parents: vec![parent.into()],
            trashed: false,
            web_view_link: None,
            shortcut_details: None,
        };

        db.upsert_root(&rf("root_id", "Google Drive", "", crate::drive::FOLDER_MIME)).unwrap();
        db.upsert(&rf("DOC1", "MyDoc", "root_id", "application/vnd.google-apps.document")).unwrap();
        db.upsert(&rf("FOL1", "Photos", "root_id", crate::drive::FOLDER_MIME)).unwrap();

        let mount = Path::new("/home/user/GoogleDrive");

        // 1. Shortcut to local folder
        let mut sc_fol = item("SC_F", "Photos Shortcut", "application/vnd.google-apps.shortcut");
        sc_fol.web_link = Some("https://drive.google.com/drive/folders/FOL1".into());
        let res_fol = desktop_link_resolved(&sc_fol, &db, mount);
        assert!(res_fol.contains("URL=file:///home/user/GoogleDrive/Photos"));
        assert!(res_fol.contains("Icon=folder-link"));

        // 2. Shortcut to Google Doc (must open browser)
        let mut sc_doc = item("SC_D", "Doc Shortcut", "application/vnd.google-apps.shortcut");
        sc_doc.web_link = Some("https://drive.google.com/open?id=DOC1".into());
        let res_doc = desktop_link_resolved(&sc_doc, &db, mount);
        assert!(res_doc.contains("URL=https://drive.google.com/open?id=DOC1"));
        assert!(res_doc.contains("Icon=x-office-document"));

        // 3. Shortcut to remote item not in local DB (fallback to web)
        let mut sc_ext = item("SC_X", "External Shortcut", "application/vnd.google-apps.shortcut");
        sc_ext.web_link = Some("https://drive.google.com/open?id=NON_EXISTENT".into());
        let res_ext = desktop_link_resolved(&sc_ext, &db, mount);
        assert!(res_ext.contains("URL=https://drive.google.com/open?id=NON_EXISTENT"));
    }
}
