//! Maps Drive names to valid, unique local file names within a directory.
use crate::db::Item;
use std::collections::HashSet;

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
}
