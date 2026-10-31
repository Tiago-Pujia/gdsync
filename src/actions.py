#!/usr/bin/env python3
"""
Acciones para integración de gdsync con gestores de archivos (Dolphin, etc.) y CLI.
Soporta pin/unpin recursivo de carpetas, descarga a caché, copia de enlaces, notificaciones
y listado de carpetas padre y archivos sueltos sincronizados sin conexión.
"""

import sys
import os
import json
import sqlite3
import subprocess
from pathlib import Path

def get_config():
    cfg_path = Path.home() / ".config" / "gdsync" / "config.json"
    if cfg_path.exists():
        try:
            with open(cfg_path, "r", encoding="utf-8") as f:
                return json.load(f)
        except Exception:
            pass
    return {}

def get_db_path():
    return Path.home() / ".local/share" / "gdsync" / "state.db"

def format_size(bytes_val):
    val = float(bytes_val or 0)
    for unit in ["B", "KB", "MB", "GB", "TB"]:
        if val < 1024.0:
            return f"{val:.1f} {unit}"
        val /= 1024.0
    return f"{val:.1f} PB"

def notify(title, message, icon="folder-gdrive"):
    try:
        subprocess.Popen(
            ["notify-send", "-a", "gdsync", "-i", icon, title, message],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
    except Exception:
        pass

def resolve_item(conn, rel_path):
    c = conn.cursor()
    c.execute("SELECT id, ino, name, mime, pinned, web_link FROM items WHERE ino=1")
    root = c.fetchone()
    if not root:
        return None
    if not rel_path or rel_path == ".":
        return root
    curr_id = root[0]
    curr_item = root
    parts = Path(rel_path).parts
    for seg in parts:
        c.execute(
            "SELECT id, ino, name, mime, pinned, web_link FROM items WHERE parent_id=? AND name=?",
            (curr_id, seg),
        )
        row = c.fetchone()
        if not row:
            return None
        curr_id = row[0]
        curr_item = row
    return curr_item

def copy_to_clipboard(text):
    for cmd in [["wl-copy"], ["xclip", "-selection", "clipboard"], ["xsel", "--clipboard", "--input"]]:
        try:
            p = subprocess.Popen(cmd, stdin=subprocess.PIPE)
            p.communicate(input=text.encode("utf-8"))
            if p.returncode == 0:
                return True
        except Exception:
            continue
    return False

def get_root_pinned_items(conn):
    """
    Obtiene únicamente las carpetas padre y los archivos sueltos anclados.
    Descarta cualquier elemento cuyo padre o ancestro ya esté anclado.
    """
    query = """
    WITH RECURSIVE
    path_builder(id, name, parent_id, mime, size, full_path) AS (
        SELECT id, name, parent_id, mime, size, name as full_path
        FROM items
        WHERE parent_id = (SELECT id FROM items WHERE ino=1)
        UNION ALL
        SELECT i.id, i.name, i.parent_id, i.mime, i.size, pb.full_path || '/' || i.name
        FROM items i JOIN path_builder pb ON i.parent_id = pb.id
    ),
    ancestors(item_id, ancestor_id, ancestor_pinned) AS (
        SELECT i.id, i.parent_id, p.pinned
        FROM items i
        JOIN items p ON i.parent_id = p.id
        UNION ALL
        SELECT a.item_id, p.parent_id, p.pinned
        FROM ancestors a
        JOIN items p ON a.ancestor_id = p.id
        WHERE p.parent_id IS NOT NULL
    )
    SELECT pb.id, pb.name, pb.mime, pb.size, pb.full_path
    FROM path_builder pb
    JOIN items i ON pb.id = i.id
    WHERE i.pinned = 1
      AND NOT EXISTS (
          SELECT 1 FROM ancestors a
          WHERE a.item_id = pb.id AND a.ancestor_pinned = 1
      )
    ORDER BY (pb.mime = 'application/vnd.google-apps.folder') DESC, pb.full_path ASC;
    """
    cur = conn.cursor()
    cur.execute(query)
    rows = cur.fetchall()

    results = []
    FOLDER_MIME = "application/vnd.google-apps.folder"
    for fid, name, mime, size, full_path in rows:
        is_folder = (mime == FOLDER_MIME)
        if is_folder:
            cur.execute("""
                WITH RECURSIVE sub(id, size, mime) AS (
                    SELECT id, size, mime FROM items WHERE id = ?
                    UNION ALL
                    SELECT i.id, i.size, i.mime FROM items i JOIN sub s ON i.parent_id = s.id
                )
                SELECT 
                    COUNT(CASE WHEN mime != 'application/vnd.google-apps.folder' AND mime NOT LIKE 'application/vnd.google-apps.%' THEN 1 END),
                    COALESCE(SUM(CASE WHEN mime != 'application/vnd.google-apps.folder' AND mime NOT LIKE 'application/vnd.google-apps.%' THEN size END), 0),
                    COUNT(CASE WHEN id IN (SELECT id FROM cache) AND mime != 'application/vnd.google-apps.folder' THEN 1 END)
                FROM sub;
            """, (fid,))
            file_count, total_size, cached_count = cur.fetchone()
            results.append({
                "id": fid,
                "name": name,
                "mime": mime,
                "is_folder": True,
                "full_path": full_path,
                "file_count": file_count,
                "total_size": total_size,
                "cached_count": cached_count,
            })
        else:
            cur.execute("SELECT 1 FROM cache WHERE id=?", (fid,))
            is_cached = cur.fetchone() is not None
            results.append({
                "id": fid,
                "name": name,
                "mime": mime,
                "is_folder": False,
                "full_path": full_path,
                "file_count": 1,
                "total_size": size or 0,
                "cached_count": 1 if is_cached else 0,
            })
    return results

def action_list_pinned(mount_point, db_path):
    if not db_path.exists():
        print("Error: base de datos no encontrada.")
        return 1

    conn = sqlite3.connect(db_path)
    items = get_root_pinned_items(conn)
    conn.close()

    if not items:
        print("=== Elementos disponibles sin conexión (anclados) ===")
        print("No hay carpetas ni archivos anclados actualmente.")
        print("\nPara mantener carpetas o archivos sin conexión, podés:")
        print("  - Clic derecho en Dolphin -> 'Mantener siempre en este equipo'")
        print("  - Ejecutar: gdsync pin <ruta>")
        return 0

    folders_count = sum(1 for it in items if it["is_folder"])
    files_count = sum(1 for it in items if not it["is_folder"])
    total_bytes = sum(it["total_size"] for it in items)
    total_files_in_all = sum(it["file_count"] for it in items)

    print("=== Elementos disponibles sin conexión (anclados) ===")
    print(f"Resumen: {folders_count} carpeta(s) padre, {files_count} archivo(s) suelto(s) ({total_files_in_all} archivos totales, {format_size(total_bytes)})\n")

    header = f"{'TIPO':<10} {'UBICACIÓN / NOMBRE':<42} {'ARCHIVOS':<12} {'TAMAÑO':<12} {'ESTADO EN CACHÉ'}"
    print(header)
    print("-" * len(header))

    for it in items:
        tipo = "📁 Carpeta" if it["is_folder"] else "📄 Archivo"
        ruta = it["full_path"]
        if len(ruta) > 40:
            ruta = "..." + ruta[-37:]

        if it["is_folder"]:
            cnt_str = f"{it['file_count']} arch."
            pct = (it["cached_count"] / it["file_count"] * 100) if it["file_count"] else 100
            cache_str = f"{it['cached_count']}/{it['file_count']} en caché ({pct:.0f}%)"
        else:
            cnt_str = "1 arch."
            cache_str = "En caché (100%)" if it["cached_count"] == 1 else "Pendiente"

        size_str = format_size(it["total_size"])
        print(f"{tipo:<10} {ruta:<42} {cnt_str:<12} {size_str:<12} {cache_str}")

    print("\nComandos útiles:")
    print("  gdsync pin <ruta>      # Mantener carpeta o archivo sin conexión")
    print("  gdsync unpin <ruta>    # Liberar espacio local (desanclar)")
    return 0

def action_pin(mount_point, db_path, target_paths):
    if not db_path.exists():
        notify("Google Drive", "Error: No se encontró la base de datos de gdsync", "dialog-error")
        print("Error: base de datos no encontrada.")
        return 1

    conn = sqlite3.connect(db_path)
    cur = conn.cursor()

    for p in target_paths:
        path_obj = Path(p).resolve()
        try:
            rel = path_obj.relative_to(mount_point)
            rel_str = str(rel)
        except ValueError:
            rel_str = str(path_obj)

        item = resolve_item(conn, rel_str)
        if not item:
            msg = f"No se encontró '{path_obj.name}' en Google Drive local."
            print(f"! {msg}")
            notify("Google Drive", msg, "dialog-warning")
            continue

        item_id, ino, name, mime, pinned, _ = item
        is_folder = (mime == "application/vnd.google-apps.folder")

        if is_folder:
            notify("Google Drive", f"Sincronizando carpeta '{name}' para uso sin conexión...", "folder-gdrive")
            print(f"Marcando carpeta '{name}' y sus contenidos como disponible sin conexión...")

            # 1. Marcar recursivamente la carpeta y todos sus descendientes en SQLite
            cur.execute("""
                WITH RECURSIVE subfolder(id) AS (
                    SELECT id FROM items WHERE id = ?
                    UNION ALL
                    SELECT i.id FROM items i JOIN subfolder s ON i.parent_id = s.id
                )
                UPDATE items SET pinned = 1 WHERE id IN (SELECT id FROM subfolder)
            """, (item_id,))
            conn.commit()

            # 2. Obtener la lista de archivos que necesitan descargarse a la caché
            cur.execute("""
                WITH RECURSIVE path_builder(id, name, parent_id, mime, full_path) AS (
                    SELECT id, name, parent_id, mime, name as full_path
                    FROM items
                    WHERE parent_id = (SELECT id FROM items WHERE ino=1)
                    UNION ALL
                    SELECT i.id, i.name, i.parent_id, i.mime, pb.full_path || '/' || i.name
                    FROM items i JOIN path_builder pb ON i.parent_id = pb.id
                )
                SELECT id, name, mime, full_path FROM path_builder
                WHERE (full_path = ? OR full_path LIKE ? || '/%')
                  AND mime != 'application/vnd.google-apps.folder'
                  AND mime NOT LIKE 'application/vnd.google-apps.%'
            """, (rel_str, rel_str))
            all_files = cur.fetchall()

            total_files = len(all_files)
            downloaded = 0

            # Descargar cada archivo que falte
            for fid, fname, fmime, fpath in all_files:
                cur.execute("SELECT 1 FROM cache WHERE id=?", (fid,))
                already_cached = cur.fetchone() is not None
                if not already_cached:
                    file_full_path = mount_point / fpath
                    print(f"  Descargando: {fname}...")
                    subprocess.run(["gdsync-bin", "pin", str(file_full_path)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
                downloaded += 1

            notify(
                "Google Drive",
                f"✓ '{name}' ({total_files} archivo{'s' if total_files != 1 else ''}) ya disponible sin conexión.",
                "emblem-checked",
            )
            print(f"✓ Carpeta '{name}' ({total_files} archivos) completada.")
        else:
            notify("Google Drive", f"Descargando '{name}' para uso sin conexión...", "folder-gdrive")
            print(f"Descargando '{name}' a la caché local...")
            res = subprocess.run(["gdsync-bin", "pin", str(path_obj)])
            if res.returncode == 0:
                notify("Google Drive", f"✓ '{name}' disponible sin conexión.", "emblem-checked")
            else:
                notify("Google Drive", f"Error al descargar '{name}'.", "dialog-error")

    conn.close()
    return 0

def action_unpin(mount_point, db_path, target_paths):
    if not db_path.exists():
        notify("Google Drive", "Error: No se encontró la base de datos de gdsync", "dialog-error")
        return 1

    conn = sqlite3.connect(db_path)
    cur = conn.cursor()

    for p in target_paths:
        path_obj = Path(p).resolve()
        try:
            rel = path_obj.relative_to(mount_point)
            rel_str = str(rel)
        except ValueError:
            rel_str = str(path_obj)

        item = resolve_item(conn, rel_str)
        if not item:
            continue

        item_id, ino, name, mime, pinned, _ = item
        is_folder = (mime == "application/vnd.google-apps.folder")

        if is_folder:
            cur.execute("""
                WITH RECURSIVE subfolder(id) AS (
                    SELECT id FROM items WHERE id = ?
                    UNION ALL
                    SELECT i.id FROM items i JOIN subfolder s ON i.parent_id = s.id
                )
                UPDATE items SET pinned = 0 WHERE id IN (SELECT id FROM subfolder)
            """, (item_id,))
            conn.commit()
            notify("Google Drive", f"☁ Espacio local liberado para '{name}' y sus carpetas.", "folder-cloud")
            print(f"✓ '{name}' desmarcado (espacio local liberable).")
        else:
            subprocess.run(["gdsync-bin", "unpin", str(path_obj)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            notify("Google Drive", f"☁ Espacio local liberado para '{name}'.", "folder-cloud")
            print(f"✓ '{name}' desmarcado (espacio local liberable).")

    conn.close()
    return 0

def action_copy_link(mount_point, db_path, target_paths):
    if not db_path.exists():
        return 1
    conn = sqlite3.connect(db_path)
    for p in target_paths:
        path_obj = Path(p).resolve()
        try:
            rel_str = str(path_obj.relative_to(mount_point))
        except ValueError:
            rel_str = str(path_obj)

        item = resolve_item(conn, rel_str)
        if item:
            fid, _, _, _, _, web_link = item
            url = web_link or f"https://drive.google.com/open?id={fid}"
            copy_to_clipboard(url)
            print(url)
            notify("Google Drive", f"Enlace copiado al portapapeles:\n{path_obj.name}", "edit-copy")
    conn.close()
    return 0

def action_open_web(mount_point, db_path, target_paths):
    if not db_path.exists():
        return 1
    conn = sqlite3.connect(db_path)
    for p in target_paths:
        path_obj = Path(p).resolve()
        try:
            rel_str = str(path_obj.relative_to(mount_point))
        except ValueError:
            rel_str = str(path_obj)

        item = resolve_item(conn, rel_str)
        if item:
            fid, _, _, _, _, web_link = item
            url = web_link or f"https://drive.google.com/open?id={fid}"
            print(f"Abriendo {url}...")
            subprocess.Popen(["xdg-open", url], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    conn.close()
    return 0

def main():
    if len(sys.argv) < 2:
        subprocess.run(["gdsync-bin", "--help"])
        return 0

    cmd = sys.argv[1]
    args = sys.argv[2:]

    cfg = get_config()
    mount_point = Path(cfg.get("mount_point", str(Path.home() / "GoogleDrive"))).resolve()
    db_path = get_db_path()

    if cmd in ("pinned", "offline"):
        return action_list_pinned(mount_point, db_path)
    elif cmd == "pin":
        if not args or args[0] in ("-l", "--list", "list"):
            return action_list_pinned(mount_point, db_path)
        return action_pin(mount_point, db_path, args)
    elif cmd == "unpin":
        return action_unpin(mount_point, db_path, args)
    elif cmd == "copy-link":
        return action_copy_link(mount_point, db_path, args)
    elif cmd == "open-web":
        return action_open_web(mount_point, db_path, args)
    else:
        # Reenviar cualquier otro comando a gdsync-bin
        return subprocess.run(["gdsync-bin", cmd] + args).returncode

if __name__ == "__main__":
    sys.exit(main() or 0)
