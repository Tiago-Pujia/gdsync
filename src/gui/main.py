#!/usr/bin/env python3
"""
gdsync-gui: Interfaz gráfica oficial para gdsync en KDE Plasma / Linux.
Construida con PyQt6 para integrarse de forma 100% nativa con KDE y el sistema.
Aprovecha al máximo todos los comandos de la CLI de gdsync:
- Monitor de estado y cuotas en tiempo real
- Explorador interactivo de Drive con resolución exacta de rutas (pin, unpin, open-web, copy-link)
- Cola de sincronización y estado en vivo
- Selección selectiva de carpetas (con carga asíncrona sin bloqueos)
- Asistente de cuenta: Importación de JSON de Google Cloud (client_secret_*.json) y login OAuth
- Ajustes de montaje, caché, sondeo y resync seguro
- Visor de registros del servicio (logs)
- Icono en bandeja del sistema (System Tray) y control de instancia única
"""

import sys
import os
import time
import json
import sqlite3
import shutil
import subprocess
from datetime import datetime
from pathlib import Path

from PyQt6.QtCore import Qt, QTimer, QSize, QThread, pyqtSignal, QLockFile
from PyQt6.QtGui import QIcon, QFont, QAction, QColor, QCursor
from PyQt6.QtWidgets import (
    QApplication, QMainWindow, QWidget, QVBoxLayout, QHBoxLayout,
    QTabWidget, QLabel, QPushButton, QProgressBar, QTreeWidget,
    QTreeWidgetItem, QTableWidget, QTableWidgetItem, QHeaderView,
    QFileDialog, QSpinBox, QLineEdit, QMessageBox, QSystemTrayIcon,
    QMenu, QGroupBox, QStyle, QTextEdit, QTextBrowser, QComboBox, QSplitter,
    QDialog, QDialogButtonBox, QAbstractItemView, QCheckBox
)
from PyQt6.QtNetwork import QLocalServer, QLocalSocket

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))
from actions import get_root_pinned_items

# ---- RUTAS ESTÁNDAR ----
def config_dir():
    return Path.home() / ".config" / "gdsync"

def config_path():
    return config_dir() / "config.json"

def token_path():
    return config_dir() / "token.json"

def autostart_desktop_path() -> Path:
    return Path.home() / ".config" / "autostart" / "gdsync-gui.desktop"

def is_gui_autostart_enabled() -> bool:
    p = autostart_desktop_path()
    if not p.exists():
        return False
    try:
        content = p.read_text(encoding="utf-8")
        if "X-GNOME-Autostart-enabled=false" in content or "Hidden=true" in content:
            return False
        return True
    except Exception:
        return False

def set_gui_autostart(enabled: bool):
    p = autostart_desktop_path()
    if enabled:
        p.parent.mkdir(parents=True, exist_ok=True)
        content = (
            "[Desktop Entry]\n"
            "Type=Application\n"
            "Name=gdsync\n"
            "GenericName=Cliente de Google Drive\n"
            "Comment=Google Drive sincronizado en tu gestor de archivos\n"
            "Exec=gdsync gui --minimized\n"
            "Icon=gdsync\n"
            "Terminal=false\n"
            "Categories=Network;FileTransfer;Utility;\n"
            "StartupNotify=false\n"
            "StartupWMClass=gdsync\n"
            "X-GNOME-Autostart-enabled=true\n"
        )
        p.write_text(content, encoding="utf-8")
    else:
        if p.exists():
            try:
                p.unlink()
            except Exception:
                pass

def load_app_icon() -> QIcon:
    # 1. Intentar tema del sistema
    icon = QIcon.fromTheme("gdsync")
    if not icon.isNull():
        return icon
    # 2. Intentar archivo directo en directorios estándar de gdsync
    for p in [
        data_dir() / "icon.png",
        data_dir() / "icon.svg",
        Path.home() / ".local" / "share" / "icons" / "hicolor" / "scalable" / "apps" / "gdsync.svg",
        Path.home() / ".local" / "share" / "icons" / "hicolor" / "256x256" / "apps" / "gdsync.png",
        Path("packaging/icons/hicolor/scalable/apps/gdsync.svg"),
        Path("packaging/icons/hicolor/256x256/apps/gdsync.png"),
    ]:
        if p.exists():
            return QIcon(str(p))
    # 3. Fallback al icono estándar de unidad
    return QApplication.style().standardIcon(QStyle.StandardPixmap.SP_DriveHDIcon)

def is_daemon_autostart_enabled() -> bool:
    try:
        res = subprocess.run(
            ["systemctl", "--user", "is-enabled", "gdsyncd"],
            capture_output=True,
            text=True
        )
        return res.returncode == 0 and "enabled" in res.stdout
    except Exception:
        return False

def set_daemon_autostart(enabled: bool):
    cmd = "enable" if enabled else "disable"
    try:
        subprocess.run(["systemctl", "--user", cmd, "gdsyncd"], check=True)
    except Exception as e:
        print(f"Error al {cmd} servicio systemd: {e}")

def data_dir():
    return Path.home() / ".local" / "share" / "gdsync"

def db_path():
    return data_dir() / "state.db"

def cache_dir():
    return Path.home() / ".cache" / "gdsync" / "blobs"

def find_cli():
    candidates = [
        Path.home() / ".local" / "bin" / "gdsync",
        Path("/usr/local/bin/gdsync"),
        Path("/usr/bin/gdsync"),
        Path(__file__).resolve().parent.parent.parent / "target" / "release" / "gdsync",
        Path(__file__).resolve().parent.parent.parent / "target" / "debug" / "gdsync",
    ]
    for c in candidates:
        if c.exists() and os.access(c, os.X_OK):
            return str(c)
    w = shutil.which("gdsync")
    return w if w else "gdsync"

def find_daemon():
    candidates = [
        Path.home() / ".local" / "bin" / "gdsyncd",
        Path("/usr/local/bin/gdsyncd"),
        Path("/usr/bin/gdsyncd"),
        Path(__file__).resolve().parent.parent.parent / "target" / "release" / "gdsyncd",
        Path(__file__).resolve().parent.parent.parent / "target" / "debug" / "gdsyncd",
    ]
    for c in candidates:
        if c.exists() and os.access(c, os.X_OK):
            return str(c)
    w = shutil.which("gdsyncd")
    return w if w else "gdsyncd"

def load_config():
    p = config_path()
    if p.exists():
        try:
            with open(p, "r", encoding="utf-8") as f:
                return json.load(f)
        except Exception:
            pass
    return {
        "client_id": "",
        "client_secret": "",
        "mount_point": str(Path.home() / "GoogleDrive"),
        "poll_interval_secs": 30,
        "cache_max_gb": 10,
        "sync_folders": ["root"],
        "close_to_tray": False
    }

def save_config(cfg):
    p = config_path()
    p.parent.mkdir(parents=True, exist_ok=True)
    with open(p, "w", encoding="utf-8") as f:
        json.dump(cfg, f, indent=2)

def is_daemon_running():
    try:
        out = subprocess.check_output(["pgrep", "-f", "gdsyncd"], stderr=subprocess.DEVNULL)
        return bool(out.strip())
    except Exception:
        return False

def is_logged_in():
    return token_path().exists()

def format_bytes(n):
    if n is None or n < 0:
        return "0 B"
    for unit in ["B", "KB", "MB", "GB", "TB"]:
        if n < 1024.0:
            return f"{n:.1f} {unit}"
        n /= 1024.0
    return f"{n:.1f} PB"

def parse_google_credentials_json(path_or_str):
    p = Path(path_or_str)
    if p.exists():
        with open(p, "r", encoding="utf-8") as f:
            data = json.load(f)
    else:
        data = json.loads(path_or_str)

    node = data.get("installed") or data.get("web") or data
    cid = node.get("client_id", "").strip()
    secret = node.get("client_secret", "").strip()
    if not cid or not secret:
        raise ValueError("El archivo JSON no contiene los campos 'client_id' y 'client_secret'.")
    return cid, secret

def ensure_dolphin_bookmark(mount_point):
    places_file = Path.home() / ".local" / "share" / "user-places.xbel"
    if not places_file.exists():
        return
    try:
        content = places_file.read_text(encoding="utf-8")
        target_uri = f"file://{Path(mount_point).resolve()}"
        if target_uri in content or "GoogleDrive" in content:
            return
        bookmark_xml = f"""  <bookmark href="{target_uri}">
   <title>Google Drive</title>
   <info>
    <metadata owner="http://freedesktop.org">
     <bookmark:icon name="folder-gdrive"/>
    </metadata>
    <metadata owner="http://www.kde.org">
     <ID>gdsync-googledrive</ID>
     <isSystemItem>false</isSystemItem>
    </metadata>
   </info>
  </bookmark>
</xbel>"""
        if "</xbel>" in content:
            updated = content.replace("</xbel>", bookmark_xml)
            places_file.write_text(updated, encoding="utf-8")
    except Exception:
        pass


# ---- HILOS EN SEGUNDO PLANO ----
class LoginWorker(QThread):
    finished_signal = pyqtSignal(bool, str)

    def __init__(self, cli_bin, client_id=None, client_secret=None, json_path=None):
        super().__init__()
        self.cli_bin = cli_bin
        self.client_id = client_id
        self.client_secret = client_secret
        self.json_path = json_path

    def run(self):
        cmd = [self.cli_bin, "login"]
        if self.json_path:
            cmd.extend(["--credentials", str(self.json_path)])
        elif self.client_id and self.client_secret:
            cmd.extend(["--client-id", self.client_id, "--client-secret", self.client_secret])

        try:
            p = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            stdout, stderr = p.communicate(timeout=180)
            if p.returncode == 0:
                self.finished_signal.emit(True, "¡Sesión iniciada con éxito en Google Drive!")
            else:
                err = stderr.strip() or stdout.strip() or "Error durante la autenticación."
                self.finished_signal.emit(False, err)
        except subprocess.TimeoutExpired:
            self.finished_signal.emit(False, "Se agotó el tiempo de espera (3 minutos) esperando la autorización en el navegador.")
        except Exception as e:
            self.finished_signal.emit(False, str(e))


class FolderLoaderWorker(QThread):
    finished_signal = pyqtSignal(bool, str)

    def __init__(self, cli_bin):
        super().__init__()
        self.cli_bin = cli_bin

    def run(self):
        try:
            out = subprocess.check_output([self.cli_bin, "folder", "list-remote"], text=True, timeout=45)
            self.finished_signal.emit(True, out)
        except Exception as e:
            self.finished_signal.emit(False, str(e))


class PinnedItemsDialog(QDialog):
    def __init__(self, parent=None, cfg=None):
        super().__init__(parent)
        self.setWindowTitle("📌 Elementos disponibles sin conexión (Carpetas y archivos)")
        self.resize(880, 500)
        self.cfg = cfg or {}
        self.mount_point = Path(self.cfg.get("mount_point", str(Path.home() / "GoogleDrive")))

        layout = QVBoxLayout(self)

        # Header con resumen
        self.lbl_summary = QLabel("Cargando elementos...")
        self.lbl_summary.setStyleSheet("font-weight: bold; font-size: 13px; margin: 4px 0;")
        layout.addWidget(self.lbl_summary)

        # Barra de búsqueda
        search_box = QHBoxLayout()
        self.txt_search = QLineEdit()
        self.txt_search.setPlaceholderText("🔍 Filtrar por nombre o ubicación...")
        self.txt_search.textChanged.connect(self.filter_items)
        search_box.addWidget(self.txt_search)

        btn_refresh = QPushButton("Actualizar")
        btn_refresh.setIcon(self.style().standardIcon(QStyle.StandardPixmap.SP_BrowserReload))
        btn_refresh.clicked.connect(self.load_items)
        search_box.addWidget(btn_refresh)
        layout.addLayout(search_box)

        # Tabla de elementos
        self.table = QTableWidget()
        self.table.setColumnCount(5)
        self.table.setHorizontalHeaderLabels([
            "Tipo", "Ubicación / Nombre", "Archivos contenidos", "Tamaño total", "Estado en caché local"
        ])
        self.table.horizontalHeader().setSectionResizeMode(0, QHeaderView.ResizeMode.ResizeToContents)
        self.table.horizontalHeader().setSectionResizeMode(1, QHeaderView.ResizeMode.Stretch)
        self.table.horizontalHeader().setSectionResizeMode(2, QHeaderView.ResizeMode.ResizeToContents)
        self.table.horizontalHeader().setSectionResizeMode(3, QHeaderView.ResizeMode.ResizeToContents)
        self.table.horizontalHeader().setSectionResizeMode(4, QHeaderView.ResizeMode.ResizeToContents)
        self.table.setSelectionBehavior(QAbstractItemView.SelectionBehavior.SelectRows)
        self.table.setEditTriggers(QAbstractItemView.EditTrigger.NoEditTriggers)
        self.table.doubleClicked.connect(self.on_open_local)
        layout.addWidget(self.table)

        # Barra de botones inferiores
        btn_bar = QHBoxLayout()
        self.btn_open_local = QPushButton("📂 Abrir en explorador")
        self.btn_open_local.clicked.connect(self.on_open_local)
        btn_bar.addWidget(self.btn_open_local)

        self.btn_open_web = QPushButton("🌐 Abrir en Drive Web")
        self.btn_open_web.clicked.connect(self.on_open_web)
        btn_bar.addWidget(self.btn_open_web)

        self.btn_unpin = QPushButton("☁️ Liberar espacio local (Unpin)")
        self.btn_unpin.clicked.connect(self.on_unpin)
        btn_bar.addWidget(self.btn_unpin)

        btn_bar.addStretch()
        btn_close = QPushButton("Cerrar")
        btn_close.clicked.connect(self.accept)
        btn_bar.addWidget(btn_close)

        layout.addLayout(btn_bar)

        self.items_data = []
        self.load_items()

    def load_items(self):
        dbp = db_path()
        if not dbp.exists():
            self.lbl_summary.setText("No se encontró la base de datos de gdsync.")
            return

        try:
            conn = sqlite3.connect(dbp)
            self.items_data = get_root_pinned_items(conn)
            conn.close()
        except Exception as e:
            self.lbl_summary.setText(f"Error al leer datos: {e}")
            return

        folders_cnt = sum(1 for it in self.items_data if it["is_folder"])
        files_cnt = sum(1 for it in self.items_data if not it["is_folder"])
        total_sz = sum(it["total_size"] for it in self.items_data)
        total_files = sum(it["file_count"] for it in self.items_data)

        self.lbl_summary.setText(
            f"Elementos anclados: {folders_cnt} carpeta(s) padre, {files_cnt} archivo(s) suelto(s) "
            f"({total_files} archivos en total, {format_bytes(total_sz)})"
        )
        self.filter_items()

    def filter_items(self):
        q = self.txt_search.text().lower().strip()
        filtered = [
            it for it in self.items_data
            if not q or q in it["name"].lower() or q in it["full_path"].lower()
        ]
        self.populate_table(filtered)

    def populate_table(self, items):
        self.table.setRowCount(len(items))
        for row_idx, it in enumerate(items):
            tipo = "📁 Carpeta" if it["is_folder"] else "📄 Archivo"
            item_tipo = QTableWidgetItem(tipo)

            item_path = QTableWidgetItem(it["full_path"])
            item_path.setData(Qt.ItemDataRole.UserRole, it)

            if it["is_folder"]:
                cnt_str = f"{it['file_count']} arch."
                pct = (it["cached_count"] / it["file_count"] * 100) if it["file_count"] else 100
                cache_str = f"{it['cached_count']}/{it['file_count']} en caché ({pct:.0f}%)"
            else:
                cnt_str = "1 arch."
                cache_str = "En caché" if it["cached_count"] == 1 else "Pendiente"

            item_cnt = QTableWidgetItem(cnt_str)
            item_cnt.setTextAlignment(Qt.AlignmentFlag.AlignCenter)

            item_size = QTableWidgetItem(format_bytes(it["total_size"]))
            item_size.setTextAlignment(Qt.AlignmentFlag.AlignRight | Qt.AlignmentFlag.AlignVCenter)

            item_cache = QTableWidgetItem(cache_str)
            if it["is_folder"] and it["cached_count"] == it["file_count"] and it["file_count"] > 0:
                item_cache.setForeground(QColor("#2e7d32"))
            elif not it["is_folder"] and it["cached_count"] == 1:
                item_cache.setForeground(QColor("#2e7d32"))
            else:
                item_cache.setForeground(QColor("#e65100"))

            self.table.setItem(row_idx, 0, item_tipo)
            self.table.setItem(row_idx, 1, item_path)
            self.table.setItem(row_idx, 2, item_cnt)
            self.table.setItem(row_idx, 3, item_size)
            self.table.setItem(row_idx, 4, item_cache)

    def get_selected_item(self):
        sel = self.table.selectedItems()
        if not sel:
            return None
        row = sel[0].row()
        item_path = self.table.item(row, 1)
        if item_path:
            return item_path.data(Qt.ItemDataRole.UserRole)
        return None

    def on_open_local(self):
        it = self.get_selected_item()
        if not it:
            return
        local_path = self.mount_point / it["full_path"]
        if local_path.exists():
            subprocess.Popen(["xdg-open", str(local_path)])
        else:
            QMessageBox.warning(self, "No disponible", f"La ruta local '{local_path}' no existe o no está montada.")

    def on_open_web(self):
        it = self.get_selected_item()
        if not it:
            return
        dbp = db_path()
        if dbp.exists():
            try:
                conn = sqlite3.connect(dbp)
                cur = conn.cursor()
                cur.execute("SELECT web_link FROM items WHERE id=?", (it["id"],))
                row = cur.fetchone()
                conn.close()
                url = (row and row[0]) or f"https://drive.google.com/open?id={it['id']}"
                subprocess.Popen(["xdg-open", url])
            except Exception as e:
                QMessageBox.warning(self, "Error", f"No se pudo abrir el enlace: {e}")

    def on_unpin(self):
        it = self.get_selected_item()
        if not it:
            return
        resp = QMessageBox.question(
            self,
            "Liberar espacio local",
            f"¿Deseás desmarcar '{it['name']}' de 'Disponible sin conexión'?\n\n"
            f"El archivo o carpeta no se borrará de Drive ni del equipo ahora, pero su espacio en caché podrá liberarse automáticamente cuando sea necesario.",
            QMessageBox.StandardButton.Yes | QMessageBox.StandardButton.No,
            QMessageBox.StandardButton.No
        )
        if resp == QMessageBox.StandardButton.Yes:
            dbp = db_path()
            if dbp.exists():
                try:
                    conn = sqlite3.connect(dbp)
                    cur = conn.cursor()
                    if it["is_folder"]:
                        cur.execute("""
                            WITH RECURSIVE sub(id) AS (
                                SELECT id FROM items WHERE id = ?
                                UNION ALL
                                SELECT i.id FROM items i JOIN sub s ON i.parent_id = s.id
                            )
                            UPDATE items SET pinned = 0 WHERE id IN (SELECT id FROM sub)
                        """, (it["id"],))
                    else:
                        cur.execute("UPDATE items SET pinned = 0 WHERE id = ?", (it["id"],))
                    conn.commit()
                    conn.close()
                except Exception as e:
                    QMessageBox.warning(self, "Error", f"No se pudo actualizar el estado: {e}")
                    return
            self.load_items()


# ---- VENTANA PRINCIPAL ----
class GdsyncWindow(QMainWindow):
    def __init__(self):
        super().__init__()
        self.setWindowTitle("gdsync - Google Drive")
        self.setWindowIcon(load_app_icon())
        self.resize(920, 660)
        self.cfg = load_config()
        self.cached_files_ids = set()
        self.previous_pending_uploads = None
        self.quota_notified = False
        self.queue_notification_id = None
        self.last_notified_pending = None
        QApplication.instance().aboutToQuit.connect(self.close_queue_notification)

        ensure_dolphin_bookmark(self.cfg.get("mount_point", str(Path.home() / "GoogleDrive")))

        central = QWidget()
        self.setCentralWidget(central)
        main_layout = QVBoxLayout(central)

        # Barra superior de estado global
        header = self.init_header()
        main_layout.addLayout(header)

        # Pestañas principales
        self.tabs = QTabWidget()
        main_layout.addWidget(self.tabs)

        self.init_tab_status()
        self.init_tab_files()
        self.init_tab_queue()
        self.init_tab_selective()
        self.init_tab_account()
        self.init_tab_settings()
        self.init_tab_logs()

        self.init_tray()

        # Timer para refrescar estado en vivo periódicamente
        self.timer = QTimer(self)
        self.timer.timeout.connect(self.refresh_status_background)
        self.timer.start(3000)

        # Carga inicial
        self.refresh_all()

    # ---- ENCABEZADO SUPERIOR ----
    def init_header(self):
        lay = QHBoxLayout()

        lbl_logo = QLabel("☁️ <b>gdsync</b>")
        lbl_logo.setStyleSheet("font-size: 16px; font-weight: bold;")
        lay.addWidget(lbl_logo)

        self.lbl_head_daemon = QLabel("Verificando...")
        self.lbl_head_daemon.setStyleSheet("padding: 3px 8px; border-radius: 4px; background: #e0e0e0;")
        lay.addWidget(self.lbl_head_daemon)

        self.lbl_head_account = QLabel("Verificando cuenta...")
        self.lbl_head_account.setStyleSheet("padding: 3px 8px; border-radius: 4px; background: #e0e0e0;")
        lay.addWidget(self.lbl_head_account)

        lay.addStretch()

        btn_dolphin = QPushButton("Abrir en Dolphin")
        btn_dolphin.setIcon(self.style().standardIcon(QStyle.StandardPixmap.SP_DirIcon))
        btn_dolphin.clicked.connect(self.open_in_dolphin)
        lay.addWidget(btn_dolphin)

        btn_refresh = QPushButton("Refrescar")
        btn_refresh.setIcon(self.style().standardIcon(QStyle.StandardPixmap.SP_BrowserReload))
        btn_refresh.clicked.connect(self.refresh_all)
        lay.addWidget(btn_refresh)

        return lay

    # ---- PESTAÑA 1: ESTADO Y MONITOR ----
    def init_tab_status(self):
        w = QWidget()
        layout = QVBoxLayout(w)

        # Grupo: Servicio Daemon
        grp_daemon = QGroupBox("Servicio en segundo plano (gdsyncd)")
        d_lay = QHBoxLayout(grp_daemon)
        self.lbl_daemon = QLabel("Estado: desconocido")
        self.btn_daemon_toggle = QPushButton("Iniciar servicio")
        self.btn_daemon_toggle.clicked.connect(self.toggle_daemon)

        self.btn_daemon_restart = QPushButton("Reiniciar servicio")
        self.btn_daemon_restart.clicked.connect(self.restart_daemon)

        d_lay.addWidget(self.lbl_daemon, stretch=1)
        d_lay.addWidget(self.btn_daemon_toggle)
        d_lay.addWidget(self.btn_daemon_restart)
        layout.addWidget(grp_daemon)

        # Grupo: Cuotas y Almacenamiento
        grp_storage = QGroupBox("Almacenamiento y Caché local")
        s_lay = QVBoxLayout(grp_storage)

        self.lbl_cloud_storage = QLabel("Almacenamiento en Google Drive:")
        self.bar_cloud = QProgressBar()
        self.bar_cloud.setValue(0)
        self.bar_cloud.setTextVisible(True)

        self.lbl_local_cache = QLabel("Caché en disco:")
        self.bar_cache = QProgressBar()
        self.bar_cache.setValue(0)
        self.bar_cache.setTextVisible(True)

        s_lay.addWidget(self.lbl_cloud_storage)
        s_lay.addWidget(self.bar_cloud)
        s_lay.addSpacing(6)
        s_lay.addWidget(self.lbl_local_cache)
        s_lay.addWidget(self.bar_cache)
        layout.addWidget(grp_storage)

        # Grupo: Resumen de Sincronización
        grp_sync = QGroupBox("Detalles de sincronización")
        sync_lay = QVBoxLayout(grp_sync)
        self.lbl_items_count = QLabel("Elementos registrados en base de datos: -")
        self.lbl_upload_queue = QLabel("Tareas pendientes de sincronización: 0")
        self.lbl_pinned_count = QLabel("Archivos disponibles sin conexión (anclados): 0")
        self.lbl_sync_mode = QLabel("Modo de sincronización: -")

        sync_lay.addWidget(self.lbl_items_count)
        sync_lay.addWidget(self.lbl_upload_queue)

        pin_row = QHBoxLayout()
        self.lbl_pinned_count = QLabel("Archivos disponibles sin conexión (anclados): 0")
        self.btn_view_pinned = QPushButton("📌 Ver carpetas y archivos sin conexión...")
        self.btn_view_pinned.setIcon(self.style().standardIcon(QStyle.StandardPixmap.SP_FileDialogContentsView))
        self.btn_view_pinned.clicked.connect(self.show_pinned_items_dialog)
        pin_row.addWidget(self.lbl_pinned_count)
        pin_row.addStretch()
        pin_row.addWidget(self.btn_view_pinned)
        sync_lay.addLayout(pin_row)

        sync_lay.addWidget(self.lbl_sync_mode)
        layout.addWidget(grp_sync)

        # Botones de accesos rápidos
        quick_box = QHBoxLayout()
        btn_web = QPushButton("Abrir Google Drive en la Web")
        btn_web.setIcon(self.style().standardIcon(QStyle.StandardPixmap.SP_FileDialogContentsView))
        btn_web.clicked.connect(self.open_drive_web_root)
        quick_box.addWidget(btn_web)

        btn_poll_now = QPushButton("Comprobar cambios remotos ahora")
        btn_poll_now.setIcon(self.style().standardIcon(QStyle.StandardPixmap.SP_BrowserReload))
        btn_poll_now.clicked.connect(self.trigger_poll_now)
        quick_box.addWidget(btn_poll_now)

        layout.addLayout(quick_box)
        layout.addStretch()
        self.tabs.addTab(w, "📊 Estado")

    # ---- PESTAÑA 2: EXPLORADOR DE ARCHIVOS DRIVE ----
    def init_tab_files(self):
        w = QWidget()
        layout = QVBoxLayout(w)

        # Barra de herramientas de búsqueda y filtros
        bar = QHBoxLayout()
        self.txt_file_search = QLineEdit()
        self.txt_file_search.setPlaceholderText("🔍 Buscar archivos o carpetas en Drive...")
        self.txt_file_search.textChanged.connect(self.filter_files_view)
        bar.addWidget(self.txt_file_search, stretch=2)

        self.cmb_file_filter = QComboBox()
        self.cmb_file_filter.addItems([
            "Todos los elementos",
            "📌 Raíces ancladas (Carpetas y archivos sueltos)",
            "📌 Todos los archivos sin conexión (Pinned)",
            "💾 En caché local",
            "☁️ Solo en la nube"
        ])
        self.cmb_file_filter.currentIndexChanged.connect(self.filter_files_view)
        bar.addWidget(self.cmb_file_filter)

        btn_reload_files = QPushButton("Actualizar lista")
        btn_reload_files.setIcon(self.style().standardIcon(QStyle.StandardPixmap.SP_BrowserReload))
        btn_reload_files.clicked.connect(self.load_files_from_db)
        bar.addWidget(btn_reload_files)

        layout.addLayout(bar)

        # Tabla de archivos
        self.table_files = QTableWidget()
        self.table_files.setColumnCount(6)
        self.table_files.setHorizontalHeaderLabels(["Nombre", "Ubicación / Carpeta", "Tamaño", "Tipo", "Estado local", "ID"])
        self.table_files.horizontalHeader().setSectionResizeMode(0, QHeaderView.ResizeMode.Interactive)
        self.table_files.setColumnWidth(0, 260)
        self.table_files.horizontalHeader().setSectionResizeMode(1, QHeaderView.ResizeMode.Stretch)
        self.table_files.horizontalHeader().setSectionResizeMode(2, QHeaderView.ResizeMode.ResizeToContents)
        self.table_files.horizontalHeader().setSectionResizeMode(3, QHeaderView.ResizeMode.ResizeToContents)
        self.table_files.horizontalHeader().setSectionResizeMode(4, QHeaderView.ResizeMode.ResizeToContents)
        self.table_files.setSelectionBehavior(QAbstractItemView.SelectionBehavior.SelectRows)
        self.table_files.setEditTriggers(QAbstractItemView.EditTrigger.NoEditTriggers)
        self.table_files.setContextMenuPolicy(Qt.ContextMenuPolicy.CustomContextMenu)
        self.table_files.customContextMenuRequested.connect(self.show_file_context_menu)
        self.table_files.doubleClicked.connect(self.on_file_double_clicked)
        layout.addWidget(self.table_files)

        # Barra de acciones inferiores para el archivo seleccionado
        act_box = QHBoxLayout()
        self.btn_file_pin = QPushButton("📌 Mantener disponible sin conexión (Pin)")
        self.btn_file_pin.clicked.connect(self.action_pin_selected)
        self.btn_file_unpin = QPushButton("☁️ Liberar espacio (Unpin)")
        self.btn_file_unpin.clicked.connect(self.action_unpin_selected)
        self.btn_file_open_web = QPushButton("🌐 Abrir en Drive Web")
        self.btn_file_open_web.clicked.connect(self.action_open_web_selected)
        self.btn_file_copy_link = QPushButton("📋 Copiar enlace")
        self.btn_file_copy_link.clicked.connect(self.action_copy_link_selected)
        self.btn_file_open_local = QPushButton("📂 Abrir localmente")
        self.btn_file_open_local.clicked.connect(self.action_open_local_selected)

        act_box.addWidget(self.btn_file_pin)
        act_box.addWidget(self.btn_file_unpin)
        act_box.addWidget(self.btn_file_open_web)
        act_box.addWidget(self.btn_file_copy_link)
        act_box.addWidget(self.btn_file_open_local)
        layout.addLayout(act_box)

        self.tabs.addTab(w, "📁 Explorador de Drive")

    # ---- PESTAÑA 3: COLA DE SINCRONIZACIÓN ----
    def init_tab_queue(self):
        w = QWidget()
        layout = QVBoxLayout(w)

        lbl = QLabel("Tareas pendientes y sincronización de cambios locales hacia Google Drive:")
        layout.addWidget(lbl)

        self.table_queue = QTableWidget()
        self.table_queue.setColumnCount(7)
        self.table_queue.setHorizontalHeaderLabels(["ID", "Operación", "Archivo / Elemento", "Tamaño", "Estado", "Intentos", "Fecha"])
        self.table_queue.horizontalHeader().setSectionResizeMode(2, QHeaderView.ResizeMode.Stretch)
        self.table_queue.setSelectionBehavior(QAbstractItemView.SelectionBehavior.SelectRows)
        self.table_queue.setEditTriggers(QAbstractItemView.EditTrigger.NoEditTriggers)
        self.table_queue.setContextMenuPolicy(Qt.ContextMenuPolicy.CustomContextMenu)
        self.table_queue.customContextMenuRequested.connect(self.show_queue_context_menu)
        self.table_queue.itemDoubleClicked.connect(self.on_queue_item_double_clicked)
        layout.addWidget(self.table_queue)

        bar = QHBoxLayout()
        self.lbl_queue_status = QLabel("Cola de sincronización: 0 tareas.")
        bar.addWidget(self.lbl_queue_status)
        bar.addStretch()

        self.btn_retry_failed = QPushButton("🔄 Reintentar fallidas")
        self.btn_retry_failed.setIcon(self.style().standardIcon(QStyle.StandardPixmap.SP_BrowserReload))
        self.btn_retry_failed.clicked.connect(self.retry_failed_uploads_clicked)
        bar.addWidget(self.btn_retry_failed)

        btn_refresh_q = QPushButton("Actualizar cola")
        btn_refresh_q.setIcon(self.style().standardIcon(QStyle.StandardPixmap.SP_BrowserReload))
        btn_refresh_q.clicked.connect(self.load_queue_from_db)
        bar.addWidget(btn_refresh_q)
        layout.addLayout(bar)

        self.tabs.addTab(w, "📤 Cola de Sincronización")

    # ---- PESTAÑA 4: SINCRONIZACIÓN SELECTIVA ----
    def init_tab_selective(self):
        w = QWidget()
        layout = QVBoxLayout(w)

        lbl = QLabel("Elegí qué carpetas querés sincronizar en tu equipo local:")
        lbl.setWordWrap(True)
        layout.addWidget(lbl)

        self.tree_folders = QTreeWidget()
        self.tree_folders.setHeaderLabels(["Carpeta remota", "ID"])
        self.tree_folders.setColumnWidth(0, 420)
        self.tree_folders.itemChanged.connect(self.on_folder_checked)
        layout.addWidget(self.tree_folders)

        btn_box = QHBoxLayout()
        self.btn_refresh_folders = QPushButton("Cargar carpetas de Google Drive")
        self.btn_refresh_folders.setIcon(self.style().standardIcon(QStyle.StandardPixmap.SP_BrowserReload))
        self.btn_refresh_folders.clicked.connect(self.load_remote_folders)

        self.btn_sync_all = QPushButton("Sincronizar toda la unidad ('root')")
        self.btn_sync_all.clicked.connect(self.sync_all_clicked)

        btn_box.addWidget(self.btn_refresh_folders)
        btn_box.addWidget(self.btn_sync_all)
        btn_box.addStretch()
        layout.addLayout(btn_box)

        self.tabs.addTab(w, "🗂️ Sincronización Selectiva")

    # ---- PESTAÑA 5: CUENTA Y CREDENCIALES ----
    def init_tab_account(self):
        w = QWidget()
        layout = QVBoxLayout(w)

        grp_tutorial = QGroupBox("Guía y tutorial de Google Cloud")
        tut_lay = QVBoxLayout(grp_tutorial)
        lbl_tut_info = QLabel(
            "¿Necesitás crear tus credenciales de Google Drive o evitar que tu sesión venza cada 7 días? "
            "Consultá la guía interactiva paso a paso:"
        )
        lbl_tut_info.setWordWrap(True)
        tut_lay.addWidget(lbl_tut_info)

        self.btn_tutorial = QPushButton("📘 Ver Tutorial Paso a Paso de Google Cloud Console")
        self.btn_tutorial.setIcon(self.style().standardIcon(QStyle.StandardPixmap.SP_MessageBoxInformation))
        self.btn_tutorial.setStyleSheet("font-weight: bold; padding: 8px; font-size: 13px;")
        self.btn_tutorial.clicked.connect(self.show_google_cloud_tutorial)
        tut_lay.addWidget(self.btn_tutorial)
        layout.addWidget(grp_tutorial)

        grp_import = QGroupBox("Importar credenciales de Google Cloud")
        imp_lay = QVBoxLayout(grp_import)
        lbl_info = QLabel(
            "Si descargaste el archivo de claves de Google Cloud Console (normalmente <b>client_secret_*.json</b> en tu carpeta de Descargas), "
            "podés importarlo directamente aquí con un solo clic:"
        )
        lbl_info.setWordWrap(True)
        imp_lay.addWidget(lbl_info)

        self.btn_import_json = QPushButton("📄 Importar archivo JSON de Google (client_secret_*.json)...")
        self.btn_import_json.setStyleSheet("font-weight: bold; padding: 8px; font-size: 13px;")
        self.btn_import_json.clicked.connect(self.import_credentials_json_dialog)
        imp_lay.addWidget(self.btn_import_json)
        layout.addWidget(grp_import)

        grp_manual = QGroupBox("Credenciales de la aplicación (OAuth 2.0)")
        m_lay = QVBoxLayout(grp_manual)

        h_id = QHBoxLayout()
        h_id.addWidget(QLabel("Client ID:"))
        self.txt_client_id = QLineEdit(self.cfg.get("client_id", ""))
        self.txt_client_id.setPlaceholderText("Ej: 1059418068302-...apps.googleusercontent.com")
        h_id.addWidget(self.txt_client_id)
        m_lay.addLayout(h_id)

        h_sec = QHBoxLayout()
        h_sec.addWidget(QLabel("Client Secret:"))
        self.txt_client_secret = QLineEdit(self.cfg.get("client_secret", ""))
        self.txt_client_secret.setEchoMode(QLineEdit.EchoMode.Password)
        self.btn_toggle_secret = QPushButton("👁️")
        self.btn_toggle_secret.setFixedWidth(36)
        self.btn_toggle_secret.clicked.connect(self.toggle_secret_visibility)
        h_sec.addWidget(self.txt_client_secret)
        h_sec.addWidget(self.btn_toggle_secret)
        m_lay.addLayout(h_sec)

        # Auto-detección amigable si no están configuradas aún
        if not self.cfg.get("client_id"):
            for sdir in [Path.home() / "Descargas", Path.home() / "Downloads"]:
                if sdir.exists():
                    found = list(sdir.glob("client_secret_*.json"))
                    if found:
                        try:
                            cid, sec = parse_google_credentials_json(found[0])
                            self.txt_client_id.setText(cid)
                            self.txt_client_secret.setText(sec)
                            self.cfg["client_id"] = cid
                            self.cfg["client_secret"] = sec
                            save_config(self.cfg)
                            break
                        except Exception:
                            pass

        btn_save_creds = QPushButton("Guardar credenciales")
        btn_save_creds.clicked.connect(self.save_credentials_manual)
        m_lay.addWidget(btn_save_creds)

        layout.addWidget(grp_manual)

        grp_auth = QGroupBox("Estado de sesión y autorización")
        a_lay = QVBoxLayout(grp_auth)
        self.lbl_auth_status = QLabel("Verificando...")
        a_lay.addWidget(self.lbl_auth_status)

        auth_btns = QHBoxLayout()
        self.btn_login = QPushButton("🔑 Iniciar Sesión con Google (OAuth 2.0)")
        self.btn_login.setStyleSheet("font-weight: bold; padding: 8px; background: #2e7d32; color: white;")
        self.btn_login.clicked.connect(self.start_login_flow)
        auth_btns.addWidget(self.btn_login)

        self.btn_logout = QPushButton("🚪 Cerrar Sesión")
        self.btn_logout.clicked.connect(self.logout_account)
        auth_btns.addWidget(self.btn_logout)

        a_lay.addLayout(auth_btns)
        layout.addWidget(grp_auth)

        layout.addStretch()
        self.tabs.addTab(w, "👤 Cuenta y Conexión")

    # ---- PESTAÑA 6: AJUSTES Y MANTENIMIENTO ----
    def init_tab_settings(self):
        w = QWidget()
        layout = QVBoxLayout(w)

        # Carpeta local de montaje
        grp_mount = QGroupBox("Carpeta de montaje local")
        mount_lay = QHBoxLayout(grp_mount)
        self.txt_mount = QLineEdit(self.cfg.get("mount_point", ""))
        btn_browse = QPushButton("Examinar...")
        btn_browse.clicked.connect(self.browse_mount)
        mount_lay.addWidget(self.txt_mount)
        mount_lay.addWidget(btn_browse)
        layout.addWidget(grp_mount)

        # Opciones de rendimiento
        grp_opts = QGroupBox("Opciones de almacenamiento y red")
        opts_lay = QVBoxLayout(grp_opts)

        h1 = QHBoxLayout()
        h1.addWidget(QLabel("Límite máximo de caché de archivos (GB):"))
        self.spin_cache = QSpinBox()
        self.spin_cache.setRange(1, 1000)
        self.spin_cache.setValue(self.cfg.get("cache_max_gb", 10))
        h1.addWidget(self.spin_cache)
        opts_lay.addLayout(h1)

        h2 = QHBoxLayout()
        h2.addWidget(QLabel("Frecuencia de sondeo de cambios (segundos):"))
        self.spin_poll = QSpinBox()
        self.spin_poll.setRange(5, 3600)
        self.spin_poll.setValue(self.cfg.get("poll_interval_secs", 30))
        h2.addWidget(self.spin_poll)
        opts_lay.addLayout(h2)

        h3 = QHBoxLayout()
        h3.addWidget(QLabel("Reintento automático de tareas fallidas (minutos, 0 = desactivado):"))
        self.spin_auto_retry = QSpinBox()
        self.spin_auto_retry.setRange(0, 1440)
        self.spin_auto_retry.setValue(self.cfg.get("auto_retry_secs", 300) // 60)
        h3.addWidget(self.spin_auto_retry)
        opts_lay.addLayout(h3)

        lbl_retry_hint = QLabel("  Reintenta periódicamente en segundo plano las subidas que hayan fallado por errores temporales de red o credenciales.")
        lbl_retry_hint.setStyleSheet("color: gray; font-size: 11px;")
        opts_lay.addWidget(lbl_retry_hint)

        layout.addWidget(grp_opts)

        # Inicio automático con el sistema
        grp_autostart = QGroupBox("Inicio automático al iniciar sesión")
        auto_lay = QVBoxLayout(grp_autostart)

        self.chk_autostart_gui = QCheckBox("Iniciar la aplicación (Bandeja del sistema) al encender la computadora")
        self.chk_autostart_gui.setChecked(is_gui_autostart_enabled())
        self.chk_autostart_gui.toggled.connect(self.on_autostart_gui_toggled)
        auto_lay.addWidget(self.chk_autostart_gui)

        lbl_gui_hint = QLabel("  Se inicia minimizada en la bandeja del sistema lista para usar.")
        lbl_gui_hint.setStyleSheet("color: gray; font-size: 11px;")
        auto_lay.addWidget(lbl_gui_hint)

        self.chk_autostart_daemon = QCheckBox("Iniciar el servicio de sincronización (gdsyncd) en segundo plano")
        self.chk_autostart_daemon.setChecked(is_daemon_autostart_enabled())
        self.chk_autostart_daemon.toggled.connect(self.on_autostart_daemon_toggled)
        auto_lay.addWidget(self.chk_autostart_daemon)

        lbl_daemon_hint = QLabel("  Mantiene Google Drive montado y sincronizado automáticamente mediante systemd.")
        lbl_daemon_hint.setStyleSheet("color: gray; font-size: 11px;")
        auto_lay.addWidget(lbl_daemon_hint)

        layout.addWidget(grp_autostart)

        # Notificaciones de escritorio
        grp_notify = QGroupBox("Notificaciones del sistema")
        notify_lay = QVBoxLayout(grp_notify)
        self.chk_notifications = QCheckBox("Mostrar notificaciones de escritorio ante eventos y subidas")
        self.chk_notifications.setChecked(self.cfg.get("notifications_enabled", True))
        self.chk_notifications.toggled.connect(self.on_notifications_toggled)
        notify_lay.addWidget(self.chk_notifications)
        lbl_notify_hint = QLabel("  Te avisa cuando finaliza la subida de archivos o si tu cuota de Drive está casi llena.")
        lbl_notify_hint.setStyleSheet("color: gray; font-size: 11px;")
        notify_lay.addWidget(lbl_notify_hint)

        self.chk_persistent_notify = QCheckBox("Notificación fija en el escritorio con tareas pendientes de subida")
        self.chk_persistent_notify.setChecked(self.cfg.get("persistent_upload_notify", True))
        self.chk_persistent_notify.toggled.connect(self.on_persistent_notify_toggled)
        notify_lay.addWidget(self.chk_persistent_notify)
        lbl_persist_hint = QLabel("  Mantiene una notificación persistente que se actualiza en vivo mientras haya archivos pendientes de subida.")
        lbl_persist_hint.setStyleSheet("color: gray; font-size: 11px;")
        notify_lay.addWidget(lbl_persist_hint)
        layout.addWidget(grp_notify)

        # Comportamiento de la ventana
        grp_behavior = QGroupBox("Comportamiento de la ventana")
        behavior_lay = QVBoxLayout(grp_behavior)
        self.chk_close_to_tray = QCheckBox("Permanecer en la bandeja del sistema al presionar la 'X' (cerrar ventana)")
        self.chk_close_to_tray.setChecked(self.cfg.get("close_to_tray", False))
        self.chk_close_to_tray.toggled.connect(self.on_close_to_tray_toggled)
        behavior_lay.addWidget(self.chk_close_to_tray)
        lbl_tray_hint = QLabel("  Si está desactivado, al hacer clic en 'X' la aplicación se cerrará por completo liberando toda la memoria RAM.")
        lbl_tray_hint.setStyleSheet("color: gray; font-size: 11px;")
        behavior_lay.addWidget(lbl_tray_hint)
        layout.addWidget(grp_behavior)

        btn_save = QPushButton("Guardar configuración")
        btn_save.clicked.connect(self.save_settings)
        layout.addWidget(btn_save)

        # Mantenimiento
        grp_danger = QGroupBox("Mantenimiento y diagnóstico")
        d_lay = QVBoxLayout(grp_danger)
        lbl_w = QLabel("Si tenés inconsistencias o querés forzar una sincronización limpia desde cero:")
        d_lay.addWidget(lbl_w)

        btn_resync = QPushButton("🧹 Restablecer base de datos y caché local (Resync)")
        btn_resync.setStyleSheet("color: #c62828;")
        btn_resync.clicked.connect(self.action_resync)
        d_lay.addWidget(btn_resync)
        layout.addWidget(grp_danger)

        layout.addStretch()
        self.tabs.addTab(w, "⚙️ Ajustes")

    # ---- PESTAÑA 7: REGISTROS (LOGS) ----
    def init_tab_logs(self):
        w = QWidget()
        layout = QVBoxLayout(w)

        lbl = QLabel("Registros recientes del servicio gdsyncd:")
        layout.addWidget(lbl)

        self.txt_logs = QTextEdit()
        self.txt_logs.setReadOnly(True)
        self.txt_logs.setFont(QFont("Monospace", 9))
        layout.addWidget(self.txt_logs)

        bar = QHBoxLayout()
        btn_fetch_logs = QPushButton("Actualizar registros")
        btn_fetch_logs.setIcon(self.style().standardIcon(QStyle.StandardPixmap.SP_BrowserReload))
        btn_fetch_logs.clicked.connect(self.load_system_logs)
        bar.addWidget(btn_fetch_logs)

        btn_clear_logs = QPushButton("Limpiar vista")
        btn_clear_logs.clicked.connect(self.txt_logs.clear)
        bar.addWidget(btn_clear_logs)

        bar.addStretch()
        layout.addLayout(bar)

        self.tabs.addTab(w, "📜 Registros")

    # ---- BANDEJA DEL SISTEMA (TRAY) ----
    def init_tray(self):
        self.tray = QSystemTrayIcon(self)
        self.tray.setObjectName("gdsync")
        self.tray.setIcon(load_app_icon())
        self.tray.setToolTip("gdsync - Google Drive")

        menu = QMenu()
        self.tray_status_action = QAction("gdsync - Todo sincronizado", self)
        self.tray_status_action.setEnabled(False)
        menu.addAction(self.tray_status_action)
        menu.addSeparator()

        act_open_dolphin = QAction("Abrir en Dolphin", self)
        act_open_dolphin.triggered.connect(self.open_in_dolphin)
        menu.addAction(act_open_dolphin)

        act_show = QAction("Abrir panel de control", self)
        act_show.triggered.connect(self.show_and_raise)
        menu.addAction(act_show)

        menu.addSeparator()
        act_toggle = QAction("Alternar servicio (Iniciar/Detener)", self)
        act_toggle.triggered.connect(self.toggle_daemon)
        menu.addAction(act_toggle)

        menu.addSeparator()
        act_quit = QAction("Salir de la aplicación", self)
        act_quit.triggered.connect(QApplication.instance().quit)
        menu.addAction(act_quit)

        self.tray.setContextMenu(menu)
        self.tray.activated.connect(self.on_tray_activated)
        self.tray.show()

    def notify(self, title: str, message: str, is_warning: bool = False):
        if not self.cfg.get("notifications_enabled", True):
            return
        urgency = "critical" if is_warning else "normal"
        try:
            subprocess.Popen([
                "notify-send",
                "-a", "gdsync",
                "-u", urgency,
                "-i", "gdsync",
                "-h", "string:desktop-entry:gdsync",
                title,
                message
            ])
        except Exception:
            if hasattr(self, "tray") and self.tray and self.tray.isVisible():
                icon = QSystemTrayIcon.MessageIcon.Warning if is_warning else QSystemTrayIcon.MessageIcon.Information
                self.tray.showMessage(title, message, icon, 4000)

    def update_queue_notification(self, total: int, uploads: int = 0, upload_bytes: int = 0, trashes: int = 0):
        notifications_on = self.cfg.get("notifications_enabled", True)
        persistent_on = self.cfg.get("persistent_upload_notify", True)

        if total == 0:
            status_text = "Todo sincronizado"
        elif uploads > 0 and trashes > 0:
            status_text = f"{uploads} por subir ({format_bytes(upload_bytes)}), {trashes} en papelera"
        elif uploads > 0:
            status_text = f"Subiendo {uploads} archivo(s): {format_bytes(upload_bytes)}"
        elif trashes > 0:
            status_text = f"Enviando {trashes} elemento(s) a la papelera (0 B por subir)"
        else:
            status_text = f"{total} tareas pendientes"

        # Actualizar tooltip e indicador en la bandeja del sistema (System Tray)
        if hasattr(self, "tray") and self.tray:
            if total > 0:
                self.tray.setToolTip(f"gdsync - {status_text}")
                if hasattr(self, "tray_status_action"):
                    self.tray_status_action.setText(status_text)
            else:
                self.tray.setToolTip("gdsync - Todo sincronizado")
                if hasattr(self, "tray_status_action"):
                    self.tray_status_action.setText("✅ Todo sincronizado")

        if not notifications_on or not persistent_on:
            if self.queue_notification_id is not None:
                self.close_queue_notification()
            return

        current_key = (total, uploads, upload_bytes, trashes)
        if total > 0:
            if self.queue_notification_id is not None and self.last_notified_pending == current_key:
                return

            cmd = [
                "notify-send",
                "-p",
                "-t", "0",
                "-h", "boolean:resident:true",
                "-h", "string:desktop-entry:gdsync",
                "-u", "low",
                "-a", "gdsync",
                "-i", "gdsync",
            ]
            if self.queue_notification_id:
                cmd.extend(["-r", str(self.queue_notification_id)])
            cmd.extend(["gdsync", status_text])

            try:
                res = subprocess.run(cmd, capture_output=True, text=True, timeout=3)
                if res.returncode == 0:
                    out = res.stdout.strip()
                    if out.isdigit():
                        self.queue_notification_id = int(out)
                    self.last_notified_pending = current_key
            except Exception:
                pass

        else:
            # pending == 0
            if self.queue_notification_id is not None or (self.last_notified_pending is not None and self.last_notified_pending != 0):
                cmd = [
                    "notify-send",
                    "-t", "5000",
                    "-u", "normal",
                    "-h", "string:desktop-entry:gdsync",
                    "-a", "gdsync",
                    "-i", "gdsync",
                ]
                if self.queue_notification_id:
                    cmd.extend(["-r", str(self.queue_notification_id)])
                cmd.extend(["gdsync", "Todas las tareas pendientes fueron subidas a Google Drive con éxito."])
                try:
                    subprocess.run(cmd, capture_output=True, text=True, timeout=3)
                except Exception:
                    pass
                self.queue_notification_id = None
                self.last_notified_pending = 0

    def close_queue_notification(self):
        if self.queue_notification_id is not None:
            nid = str(self.queue_notification_id)
            self.queue_notification_id = None
            self.last_notified_pending = None
            try:
                subprocess.run(
                    ["gdbus", "call", "--session", "--dest", "org.freedesktop.Notifications",
                     "--object-path", "/org/freedesktop/Notifications",
                     "--method", "org.freedesktop.Notifications.CloseNotification", nid],
                    capture_output=True,
                    timeout=2
                )
            except Exception:
                pass

    def on_tray_activated(self, reason):
        if reason == QSystemTrayIcon.ActivationReason.Trigger:
            self.show_and_raise()

    def show_and_raise(self):
        self.show()
        self.raise_()
        self.activateWindow()
        self.refresh_all()

    def hideEvent(self, event):
        super().hideEvent(event)
        # Ralentizar sondeo a 30 segundos mientras la ventana esté oculta
        if hasattr(self, "timer") and self.timer.isActive():
            self.timer.setInterval(30000)

    def showEvent(self, event):
        super().showEvent(event)
        # Reactivar sondeo a 3 segundos mientras la ventana esté visible
        if hasattr(self, "timer"):
            self.timer.setInterval(3000)
            if not self.timer.isActive():
                self.timer.start(3000)

    def closeEvent(self, event):
        close_to_tray = self.cfg.get("close_to_tray", False)
        if close_to_tray and hasattr(self, "tray") and self.tray.isVisible():
            self.hide()
            event.ignore()
        else:
            event.accept()
            QApplication.instance().quit()

    # ---- REFRESH GENERAL Y ESTADO ----
    def refresh_all(self):
        self.cfg = load_config()
        self.refresh_status_background()
        self.load_files_from_db()
        self.load_queue_from_db()
        self.load_system_logs()

    def refresh_status_background(self):
        # Si la ventana no está visible (está oculta en la bandeja),
        # evitamos el escaneo intensivo del disco y renderizado de widgets
        if not self.isVisible():
            dbp = db_path()
            if dbp.exists():
                try:
                    conn = sqlite3.connect(f"file:{dbp}?mode=ro", uri=True)
                    cur = conn.cursor()
                    cur.execute("""
                        SELECT
                            COUNT(*),
                            COALESCE(SUM(CASE WHEN q.op IN ('create', 'update') THEN 1 ELSE 0 END), 0),
                            (SELECT COALESCE(SUM(i.size), 0)
                             FROM items i
                             WHERE i.id IN (SELECT DISTINCT file_id FROM upload_queue WHERE status='pending' AND op IN ('create', 'update'))
                            ),
                            COALESCE(SUM(CASE WHEN q.op = 'trash' THEN 1 ELSE 0 END), 0)
                        FROM upload_queue q
                        WHERE q.status = 'pending'
                    """)
                    summary_row = cur.fetchone()
                    conn.close()
                    tot_pending = summary_row[0] if summary_row else 0
                    uploads_cnt = summary_row[1] if summary_row else 0
                    upload_bytes = summary_row[2] if summary_row else 0
                    trash_cnt = summary_row[3] if summary_row else 0
                    self.update_queue_notification(tot_pending, uploads_cnt, upload_bytes, trash_cnt)
                except Exception:
                    pass
            return

        running = is_daemon_running()
        logged = is_logged_in()

        # Cabecera
        if running:
            self.lbl_head_daemon.setText("🟢 Servicio activo")
            self.lbl_head_daemon.setStyleSheet("padding: 3px 8px; border-radius: 4px; background: #c8e6c9; color: #1b5e20;")
            self.lbl_daemon.setText("🟢 <b>En ejecución</b> (Sistema de archivos FUSE montado)")
            self.btn_daemon_toggle.setText("Detener servicio")
        else:
            self.lbl_head_daemon.setText("⚪ Servicio detenido")
            self.lbl_head_daemon.setStyleSheet("padding: 3px 8px; border-radius: 4px; background: #eeeeee; color: #616161;")
            self.lbl_daemon.setText("⚪ <b>Detenido</b>")
            self.btn_daemon_toggle.setText("Iniciar servicio")

        if logged:
            self.lbl_head_account.setText("🔑 Conectado")
            self.lbl_head_account.setStyleSheet("padding: 3px 8px; border-radius: 4px; background: #bbdefb; color: #0d47a1;")
            self.lbl_auth_status.setText("✅ <b>Sesión activa:</b> Conectado con token de Google válido.")
            self.btn_login.setText("Re-autenticar con Google")
            self.btn_logout.setEnabled(True)
        else:
            self.lbl_head_account.setText("🔒 No conectado")
            self.lbl_head_account.setStyleSheet("padding: 3px 8px; border-radius: 4px; background: #ffcdd2; color: #b71c1c;")
            self.lbl_auth_status.setText("⚠️ <b>Sin sesión:</b> No has iniciado sesión en Google.")
            self.btn_login.setText("🔑 Iniciar Sesión con Google (OAuth 2.0)")
            self.btn_logout.setEnabled(False)

        # Caché en disco
        cache_bytes = 0
        cached_ids = set()
        cdir = cache_dir()
        if cdir.exists():
            for f in cdir.glob("*"):
                if f.is_file():
                    cache_bytes += f.stat().st_size
                    cached_ids.add(f.name)
        self.cached_files_ids = cached_ids

        cache_mb = cache_bytes / (1024 * 1024)
        max_cache_mb = self.cfg.get("cache_max_gb", 10) * 1024
        pct_cache = min(100, int((cache_mb / max_cache_mb) * 100)) if max_cache_mb else 0
        self.lbl_local_cache.setText(f"Caché en disco: <b>{cache_mb:.1f} MB</b> ({len(cached_ids)} archivos descargados de {self.cfg.get('cache_max_gb', 10)} GB límite)")
        self.bar_cache.setValue(pct_cache)
        self.bar_cache.setFormat(f"{cache_mb:.1f} MB / {self.cfg.get('cache_max_gb', 10)} GB ({pct_cache}%)")

        # Base de datos y Cuota de Drive
        dbp = db_path()
        if dbp.exists():
            try:
                conn = sqlite3.connect(f"file:{dbp}?mode=ro", uri=True)
                cur = conn.cursor()
                cur.execute("SELECT COUNT(*) FROM items")
                items_cnt = cur.fetchone()[0]
                self.lbl_items_count.setText(f"Elementos registrados en base de datos: <b>{items_cnt}</b>")

                cur.execute("SELECT COUNT(*) FROM items WHERE pinned=1")
                pinned_cnt = cur.fetchone()[0]
                self.lbl_pinned_count.setText(f"Archivos disponibles sin conexión (anclados): <b>{pinned_cnt}</b>")

                cur.execute("""
                    SELECT
                        COUNT(CASE WHEN q.status = 'pending' THEN 1 END),
                        COALESCE(SUM(CASE WHEN q.status = 'pending' AND q.op IN ('create', 'update') THEN 1 ELSE 0 END), 0),
                        (SELECT COALESCE(SUM(i.size), 0)
                         FROM items i
                         WHERE i.id IN (SELECT DISTINCT file_id FROM upload_queue WHERE status='pending' AND op IN ('create', 'update'))
                        ),
                        COALESCE(SUM(CASE WHEN q.status = 'pending' AND q.op = 'trash' THEN 1 ELSE 0 END), 0),
                        COUNT(CASE WHEN q.status = 'failed' THEN 1 END)
                    FROM upload_queue q
                """)
                summary_row = cur.fetchone()
                tot_pending = summary_row[0] if summary_row else 0
                uploads_cnt = summary_row[1] if summary_row else 0
                upload_bytes = summary_row[2] if summary_row else 0
                trash_cnt = summary_row[3] if summary_row else 0
                failed_cnt = summary_row[4] if summary_row else 0

                failed_label = f" | <span style='color: #e53935; font-weight: bold;'>⚠️ {failed_cnt} fallida(s)</span>" if failed_cnt > 0 else ""

                if tot_pending == 0 and failed_cnt == 0:
                    self.lbl_upload_queue.setText("Tareas pendientes de sincronización: <b>0</b> (Todo al día)")
                elif tot_pending == 0:
                    self.lbl_upload_queue.setText(f"Tareas pendientes: <b>0</b>{failed_label}")
                elif uploads_cnt > 0 and trash_cnt > 0:
                    self.lbl_upload_queue.setText(
                        f"Tareas pendientes: <b>{tot_pending}</b> ({uploads_cnt} por subir: <b>{format_bytes(upload_bytes)}</b> | {trash_cnt} en papelera){failed_label}"
                    )
                elif uploads_cnt > 0:
                    self.lbl_upload_queue.setText(
                        f"Tareas pendientes de sincronización: <b>{uploads_cnt}</b> (<b>{format_bytes(upload_bytes)}</b> por subir){failed_label}"
                    )
                elif trash_cnt > 0:
                    self.lbl_upload_queue.setText(
                        f"Tareas pendientes: <b>{tot_pending}</b> ({trash_cnt} enviándose a la papelera | 0 B por subir){failed_label}"
                    )
                else:
                    self.lbl_upload_queue.setText(f"Tareas pendientes: <b>{tot_pending}</b>{failed_label}")

                self.update_queue_notification(tot_pending, uploads_cnt, upload_bytes, trash_cnt)

                cur.execute("SELECT key, value FROM kv WHERE key IN ('storage_total', 'storage_used')")
                kv_rows = dict(cur.fetchall())
                conn.close()

                if "storage_total" in kv_rows and "storage_used" in kv_rows:
                    tot = int(kv_rows["storage_total"])
                    used = int(kv_rows["storage_used"])
                    pct_cloud = min(100, int((used / tot) * 100)) if tot else 0
                    self.lbl_cloud_storage.setText(
                        f"Almacenamiento en Google Drive: <b>{format_bytes(used)}</b> usados de {format_bytes(tot)} ({pct_cloud}%)"
                    )
                    self.bar_cloud.setValue(pct_cloud)
                    self.bar_cloud.setFormat(f"{format_bytes(used)} / {format_bytes(tot)} ({pct_cloud}%)")
            except Exception:
                pass

        # Modo sincronización
        sync_folders = self.cfg.get("sync_folders", ["root"])
        if sync_folders == ["root"] or not sync_folders:
            self.lbl_sync_mode.setText("Modo de sincronización: <b>Toda la unidad (completo)</b>")
        else:
            self.lbl_sync_mode.setText(f"Modo de sincronización: <b>{len(sync_folders)} carpeta(s) selectiva(s)</b>")

    # ---- PESTAÑA EXPLORADOR DE ARCHIVOS ----
    def load_files_from_db(self):
        self.root_pinned_ids = set()
        dbp = db_path()
        if not dbp.exists():
            self.table_files.setRowCount(0)
            return

        try:
            conn = sqlite3.connect(f"file:{dbp}?mode=ro", uri=True)
            try:
                root_pinned = get_root_pinned_items(conn)
                self.root_pinned_ids = {it["id"] for it in root_pinned}
            except Exception:
                pass
            cur = conn.cursor()
            # Cargar mapa de id -> (parent_id, name) para reconstruir rutas completas
            cur.execute("SELECT id, parent_id, name FROM items")
            parent_map = {row[0]: (row[1], row[2]) for row in cur.fetchall()}

            def resolve_rel_path(item_id):
                parts = []
                curr = item_id
                visited = set()
                while curr and curr not in visited:
                    visited.add(curr)
                    if curr not in parent_map:
                        break
                    pid, name = parent_map[curr]
                    if not pid:
                        break
                    parts.append(name)
                    curr = pid
                parts.reverse()
                return "/".join(parts)

            cur.execute("""
                SELECT id, name, size, mime, pinned, web_link
                FROM items
                WHERE ino != 1
                ORDER BY (mime='application/vnd.google-apps.folder') DESC, name ASC
                LIMIT 2000
            """)
            rows = cur.fetchall()
            conn.close()

            rows_with_path = [
                (r[0], r[1], r[2], r[3], r[4], r[5], resolve_rel_path(r[0]))
                for r in rows
            ]
        except Exception:
            return

        self.all_files_data = rows_with_path
        self.populate_files_table(rows_with_path)

    def populate_files_table(self, rows):
        self.table_files.setRowCount(len(rows))
        FOLDER_MIME = "application/vnd.google-apps.folder"

        for row_idx, (fid, name, size, mime, pinned, web_link, rel_path) in enumerate(rows):
            is_folder = (mime == FOLDER_MIME)
            icon = "📁 " if is_folder else "📄 "

            if is_folder:
                status_str = "—"
            elif pinned:
                status_str = "📌 Disponible sin conexión"
            elif fid in self.cached_files_ids:
                status_str = "💾 En caché local"
            else:
                status_str = "☁️ Solo en la nube"

            size_str = "—" if is_folder else format_bytes(size)

            item_name = QTableWidgetItem(f"{icon}{name}")
            item_name.setData(Qt.ItemDataRole.UserRole, fid)
            item_name.setData(Qt.ItemDataRole.UserRole + 1, web_link)
            item_name.setData(Qt.ItemDataRole.UserRole + 2, is_folder)
            item_name.setData(Qt.ItemDataRole.UserRole + 3, name)
            item_name.setData(Qt.ItemDataRole.UserRole + 4, rel_path)

            folder_location = str(Path(rel_path).parent) if "/" in rel_path else "(Raíz)"
            item_location = QTableWidgetItem(folder_location)
            item_location.setForeground(QColor("#757575"))

            item_size = QTableWidgetItem(size_str)
            item_size.setTextAlignment(Qt.AlignmentFlag.AlignRight | Qt.AlignmentFlag.AlignVCenter)

            type_str = "Carpeta" if is_folder else mime.split("/")[-1]
            item_type = QTableWidgetItem(type_str)

            item_status = QTableWidgetItem(status_str)
            if pinned:
                item_status.setForeground(QColor("#2e7d32"))
            elif fid in self.cached_files_ids:
                item_status.setForeground(QColor("#1565c0"))
            else:
                item_status.setForeground(QColor("#757575"))

            item_id = QTableWidgetItem(fid)

            self.table_files.setItem(row_idx, 0, item_name)
            self.table_files.setItem(row_idx, 1, item_location)
            self.table_files.setItem(row_idx, 2, item_size)
            self.table_files.setItem(row_idx, 3, item_type)
            self.table_files.setItem(row_idx, 4, item_status)
            self.table_files.setItem(row_idx, 5, item_id)

    def filter_files_view(self):
        if not hasattr(self, "all_files_data"):
            return
        query = self.txt_file_search.text().lower().strip()
        filter_idx = self.cmb_file_filter.currentIndex()

        filtered = []
        for row in self.all_files_data:
            fid, name, size, mime, pinned, web_link, rel_path = row
            if query and (query not in name.lower() and query not in rel_path.lower()):
                continue

            if filter_idx == 1 and fid not in getattr(self, "root_pinned_ids", set()):
                continue
            elif filter_idx == 2 and not pinned:
                continue
            elif filter_idx == 3 and (fid not in self.cached_files_ids or pinned):
                continue
            elif filter_idx == 4 and (fid in self.cached_files_ids or pinned):
                continue

            filtered.append(row)

        self.populate_files_table(filtered)

    def show_file_context_menu(self, pos):
        item = self.table_files.itemAt(pos)
        if not item:
            return
        row = item.row()
        item_name = self.table_files.item(row, 0)
        fid = item_name.data(Qt.ItemDataRole.UserRole)
        web_link = item_name.data(Qt.ItemDataRole.UserRole + 1)
        is_folder = item_name.data(Qt.ItemDataRole.UserRole + 2)
        fname = item_name.data(Qt.ItemDataRole.UserRole + 3)
        rel_path = item_name.data(Qt.ItemDataRole.UserRole + 4) or fname

        menu = QMenu(self)
        act_pin = menu.addAction("📌 Mantener disponible sin conexión (Pin)")
        act_unpin = menu.addAction("☁️ Liberar espacio local (Unpin)")
        menu.addSeparator()
        act_open_web = menu.addAction("🌐 Abrir en Google Drive Web")
        act_copy_link = menu.addAction("📋 Copiar enlace compartible")
        menu.addSeparator()
        act_open_local = menu.addAction("📂 Abrir archivo/carpeta localmente")

        action = menu.exec(self.table_files.viewport().mapToGlobal(pos))
        if action == act_pin:
            self.pin_file_by_id(fid, fname, rel_path)
        elif action == act_unpin:
            self.unpin_file_by_id(fid, fname, rel_path)
        elif action == act_open_web:
            self.open_web_by_id(fid, web_link)
        elif action == act_copy_link:
            self.copy_link_by_id(fid, web_link)
        elif action == act_open_local:
            self.open_local_by_path(rel_path, is_folder)

    def get_selected_file_info(self):
        row = self.table_files.currentRow()
        if row < 0:
            QMessageBox.information(self, "Aviso", "Seleccioná primero un archivo o carpeta de la lista.")
            return None
        item_name = self.table_files.item(row, 0)
        return {
            "id": item_name.data(Qt.ItemDataRole.UserRole),
            "web_link": item_name.data(Qt.ItemDataRole.UserRole + 1),
            "is_folder": item_name.data(Qt.ItemDataRole.UserRole + 2),
            "name": item_name.data(Qt.ItemDataRole.UserRole + 3),
            "rel_path": item_name.data(Qt.ItemDataRole.UserRole + 4) or item_name.data(Qt.ItemDataRole.UserRole + 3)
        }

    def show_pinned_items_dialog(self):
        dlg = PinnedItemsDialog(self, cfg=self.cfg)
        dlg.exec()
        self.load_files_from_db()
        self.refresh_status_background()

    def action_pin_selected(self):
        info = self.get_selected_file_info()
        if info:
            self.pin_file_by_id(info["id"], info["name"], info["rel_path"])

    def action_unpin_selected(self):
        info = self.get_selected_file_info()
        if info:
            self.unpin_file_by_id(info["id"], info["name"], info["rel_path"])

    def action_open_web_selected(self):
        info = self.get_selected_file_info()
        if info:
            self.open_web_by_id(info["id"], info["web_link"])

    def action_copy_link_selected(self):
        info = self.get_selected_file_info()
        if info:
            self.copy_link_by_id(info["id"], info["web_link"])

    def action_open_local_selected(self):
        info = self.get_selected_file_info()
        if info:
            self.open_local_by_path(info["rel_path"], info["is_folder"])

    def pin_file_by_id(self, fid, fname, rel_path=None):
        dbp = db_path()
        if dbp.exists():
            try:
                conn = sqlite3.connect(dbp)
                conn.execute("UPDATE items SET pinned=1 WHERE id=?", (fid,))
                conn.commit()
                conn.close()
            except Exception as e:
                QMessageBox.warning(self, "Error", f"No se pudo actualizar estado en base de datos: {e}")
                return
        cli = find_cli()
        mount = Path(self.cfg.get("mount_point", str(Path.home() / "GoogleDrive")))
        target = mount / (rel_path or fname)
        if target.exists():
            subprocess.Popen([cli, "pin", str(target)])
        QMessageBox.information(self, "Pin", f"'{fname}' marcado como Disponible sin conexión.")
        self.load_files_from_db()
        self.refresh_status_background()

    def unpin_file_by_id(self, fid, fname, rel_path=None):
        dbp = db_path()
        if dbp.exists():
            try:
                conn = sqlite3.connect(dbp)
                conn.execute("UPDATE items SET pinned=0 WHERE id=?", (fid,))
                conn.commit()
                conn.close()
            except Exception as e:
                QMessageBox.warning(self, "Error", f"No se pudo actualizar estado en base de datos: {e}")
                return
        cli = find_cli()
        mount = Path(self.cfg.get("mount_point", str(Path.home() / "GoogleDrive")))
        target = mount / (rel_path or fname)
        if target.exists():
            subprocess.Popen([cli, "unpin", str(target)])
        QMessageBox.information(self, "Unpin", f"'{fname}' desmarcado. Ahora puede ser liberado de la caché.")
        self.load_files_from_db()
        self.refresh_status_background()

    def open_web_by_id(self, fid, web_link):
        url = web_link or f"https://drive.google.com/open?id={fid}"
        subprocess.Popen(["xdg-open", url])

    def copy_link_by_id(self, fid, web_link):
        url = web_link or f"https://drive.google.com/open?id={fid}"
        cb = QApplication.clipboard()
        cb.setText(url)
        QMessageBox.information(self, "Enlace copiado", f"Enlace de Google Drive copiado al portapapeles:\n{url}")

    def open_local_by_path(self, rel_path, is_folder=False):
        mount = Path(self.cfg.get("mount_point", str(Path.home() / "GoogleDrive")))
        target = mount / rel_path
        if target.exists():
            subprocess.Popen(["xdg-open", str(target)])
        else:
            parent_dir = target.parent
            if parent_dir.exists():
                subprocess.Popen(["xdg-open", str(parent_dir)])
            else:
                subprocess.Popen(["xdg-open", str(mount)])

    def on_file_double_clicked(self, index):
        info = self.get_selected_file_info()
        if info:
            self.open_local_by_path(info["rel_path"], info["is_folder"])

    # ---- PESTAÑA COLA DE SINCRONIZACIÓN ----
    def load_queue_from_db(self):
        dbp = db_path()
        if not dbp.exists():
            self.table_queue.setRowCount(0)
            self.lbl_queue_status.setText("Base de datos no inicializada.")
            if hasattr(self, "btn_retry_failed"):
                self.btn_retry_failed.setEnabled(False)
            return

        try:
            conn = sqlite3.connect(f"file:{dbp}?mode=ro", uri=True)
            cur = conn.cursor()
            cur.execute("""
                SELECT q.id, q.op, q.file_id, i.name, q.payload, i.size, q.status, q.attempts, q.created_at, q.last_error
                FROM upload_queue q
                LEFT JOIN items i ON q.file_id = i.id
                ORDER BY q.id DESC LIMIT 100
            """)
            rows = cur.fetchall()

            cur.execute("""
                SELECT
                    COUNT(CASE WHEN q.status = 'pending' THEN 1 END),
                    COALESCE(SUM(CASE WHEN q.status = 'pending' AND q.op IN ('create', 'update') THEN 1 ELSE 0 END), 0),
                    (SELECT COALESCE(SUM(i.size), 0)
                     FROM items i
                     WHERE i.id IN (SELECT DISTINCT file_id FROM upload_queue WHERE status='pending' AND op IN ('create', 'update'))
                    ),
                    COALESCE(SUM(CASE WHEN q.status = 'pending' AND q.op = 'trash' THEN 1 ELSE 0 END), 0),
                    COUNT(CASE WHEN q.status = 'failed' THEN 1 END)
                FROM upload_queue q
            """)
            summary_row = cur.fetchone()
            tot_pending = summary_row[0] if summary_row else 0
            uploads_cnt = summary_row[1] if summary_row else 0
            upload_bytes = summary_row[2] if summary_row else 0
            trash_cnt = summary_row[3] if summary_row else 0
            failed_cnt = summary_row[4] if summary_row else 0
            conn.close()
        except Exception as e:
            self.lbl_queue_status.setText(f"Error consultando cola: {e}")
            return

        self.table_queue.setRowCount(len(rows))
        for row_idx, (qid, op, fid, iname, payload, fsize, status, attempts, created, last_error) in enumerate(rows):
            dt_str = datetime.fromtimestamp(created).strftime("%Y-%m-%d %H:%M:%S") if created else "—"

            fname = iname
            if not fname and payload:
                try:
                    p = json.loads(payload)
                    fname = p.get("name")
                except Exception:
                    pass
            display_name = f"{fname} ({fid})" if fname and fname != fid else fid

            op_lower = op.lower()
            if op_lower in ("create", "update"):
                size_str = format_bytes(fsize) if fsize is not None and fsize > 0 else "0 B"
            elif op_lower == "trash":
                size_str = "Papelera (0 B)"
            elif op_lower == "mkdir":
                size_str = "Carpeta"
            else:
                size_str = "Metadatos"

            item_id = QTableWidgetItem(str(qid))
            item_op = QTableWidgetItem(op.upper())
            item_name = QTableWidgetItem(display_name)
            item_size = QTableWidgetItem(size_str)
            item_status = QTableWidgetItem(status)
            item_attempts = QTableWidgetItem(str(attempts))
            item_date = QTableWidgetItem(dt_str)

            if status == "failed":
                item_status.setForeground(QColor("#e53935"))
                font = item_status.font()
                font.setBold(True)
                item_status.setFont(font)
                if last_error:
                    tooltip = f"Fallo al sincronizar:\n{last_error}\n\n(Hacé doble clic para ver el mensaje completo)"
                    item_status.setToolTip(tooltip)
                    item_name.setToolTip(tooltip)
            elif status == "pending":
                item_status.setForeground(QColor("#fb8c00"))

            self.table_queue.setItem(row_idx, 0, item_id)
            self.table_queue.setItem(row_idx, 1, item_op)
            self.table_queue.setItem(row_idx, 2, item_name)
            self.table_queue.setItem(row_idx, 3, item_size)
            self.table_queue.setItem(row_idx, 4, item_status)
            self.table_queue.setItem(row_idx, 5, item_attempts)
            self.table_queue.setItem(row_idx, 6, item_date)

        self.previous_pending_uploads = tot_pending

        if hasattr(self, "btn_retry_failed"):
            if failed_cnt > 0:
                self.btn_retry_failed.setEnabled(True)
                self.btn_retry_failed.setText(f"🔄 Reintentar fallidas ({failed_cnt})")
                self.btn_retry_failed.setStyleSheet("font-weight: bold;")
            else:
                self.btn_retry_failed.setEnabled(False)
                self.btn_retry_failed.setText("🔄 Reintentar fallidas")
                self.btn_retry_failed.setStyleSheet("")

        if not rows:
            self.lbl_queue_status.setText("✅ Cola vacía. No hay tareas pendientes.")
        elif tot_pending == 0 and failed_cnt > 0:
            self.lbl_queue_status.setText(
                f"⚠️ Hay {failed_cnt} tarea(s) fallida(s) en la cola. Hacé clic en 'Reintentar fallidas' para volver a intentar."
            )
        elif tot_pending == 0:
            self.lbl_queue_status.setText("✅ Todo sincronizado. No hay tareas pendientes en la cola.")
        elif uploads_cnt > 0 and trash_cnt > 0:
            extra_failed = f" | ⚠️ {failed_cnt} fallidas" if failed_cnt > 0 else ""
            self.lbl_queue_status.setText(
                f"Cola: {tot_pending} pendientes ({uploads_cnt} subidas: {format_bytes(upload_bytes)}, {trash_cnt} en papelera){extra_failed}."
            )
        elif uploads_cnt > 0:
            extra_failed = f" | ⚠️ {failed_cnt} fallidas" if failed_cnt > 0 else ""
            self.lbl_queue_status.setText(
                f"Cola: {tot_pending} pendientes ({uploads_cnt} archivos: {format_bytes(upload_bytes)}){extra_failed}."
            )
        elif trash_cnt > 0:
            extra_failed = f" | ⚠️ {failed_cnt} fallidas" if failed_cnt > 0 else ""
            self.lbl_queue_status.setText(
                f"Cola: {tot_pending} pendientes ({trash_cnt} en papelera){extra_failed}."
            )
        else:
            extra_failed = f" | ⚠️ {failed_cnt} fallidas" if failed_cnt > 0 else ""
            self.lbl_queue_status.setText(
                f"Cola: {tot_pending} pendientes{extra_failed}."
            )

    def retry_failed_uploads_clicked(self):
        selected_rows = self.table_queue.selectionModel().selectedRows()
        selected_ids = []
        for index in selected_rows:
            try:
                qid = int(self.table_queue.item(index.row(), 0).text())
                selected_ids.append(qid)
            except Exception:
                pass

        dbp = db_path()
        if not dbp.exists():
            return

        try:
            conn = sqlite3.connect(dbp)
            cur = conn.cursor()
            if selected_ids and len(selected_ids) < self.table_queue.rowCount():
                placeholders = ",".join("?" for _ in selected_ids)
                cur.execute(
                    f"UPDATE upload_queue SET status='pending', attempts=0, last_error=NULL WHERE id IN ({placeholders})",
                    selected_ids,
                )
                count = cur.rowcount
            else:
                cur.execute("UPDATE upload_queue SET status='pending', attempts=0, last_error=NULL WHERE status='failed'")
                count = cur.rowcount
            conn.commit()
            conn.close()

            # Despertar al daemon gdsyncd de inmediato
            subprocess.call(["pkill", "-USR1", "-x", "gdsyncd"])

            self.load_queue_from_db()
            if count > 0:
                self.notify("gdsync", f"Se reactivaron {count} tarea(s) para reintentar la subida.")
            else:
                QMessageBox.information(self, "Cola de Sincronización", "No se encontraron tareas fallidas para reintentar.")
        except Exception as e:
            QMessageBox.critical(self, "Error", f"No se pudieron reactivar las tareas: {e}")

    def show_queue_context_menu(self, pos):
        selected_rows = self.table_queue.selectionModel().selectedRows()
        if not selected_rows:
            return

        menu = QMenu(self)
        act_retry = menu.addAction("🔄 Reintentar tarea(s) seleccionada(s)")
        act_retry.triggered.connect(self.retry_selected_tasks)

        act_discard = menu.addAction("🗑️ Descartar tarea(s) de la cola")
        act_discard.triggered.connect(self.discard_selected_tasks)

        menu.addSeparator()
        act_details = menu.addAction("ℹ️ Ver detalle del error")
        act_details.triggered.connect(self.show_selected_task_error)

        menu.exec(QCursor.pos())

    def on_queue_item_double_clicked(self, item):
        self.show_selected_task_error()

    def show_selected_task_error(self):
        selected_rows = self.table_queue.selectionModel().selectedRows()
        if not selected_rows:
            return
        row = selected_rows[0].row()
        qid_str = self.table_queue.item(row, 0).text()
        name_str = self.table_queue.item(row, 2).text()
        status_str = self.table_queue.item(row, 4).text()

        dbp = db_path()
        error_msg = "Sin mensaje de error registrado."
        if dbp.exists():
            try:
                conn = sqlite3.connect(f"file:{dbp}?mode=ro", uri=True)
                cur = conn.cursor()
                cur.execute("SELECT last_error FROM upload_queue WHERE id=?", (int(qid_str),))
                res = cur.fetchone()
                if res and res[0]:
                    error_msg = res[0]
                conn.close()
            except Exception as e:
                error_msg = str(e)

        dlg = QMessageBox(self)
        dlg.setWindowTitle(f"Detalle de Tarea #{qid_str}")
        dlg.setText(f"<b>Elemento:</b> {name_str}<br><b>Estado:</b> {status_str}")
        dlg.setDetailedText(error_msg)
        if status_str == "failed":
            dlg.setIcon(QMessageBox.Icon.Warning)
        else:
            dlg.setIcon(QMessageBox.Icon.Information)
        dlg.exec()

    def retry_selected_tasks(self):
        selected_rows = self.table_queue.selectionModel().selectedRows()
        if not selected_rows:
            return
        ids = [int(self.table_queue.item(r.row(), 0).text()) for r in selected_rows]
        dbp = db_path()
        if not dbp.exists():
            return
        try:
            conn = sqlite3.connect(dbp)
            cur = conn.cursor()
            placeholders = ",".join("?" for _ in ids)
            cur.execute(
                f"UPDATE upload_queue SET status='pending', attempts=0, last_error=NULL WHERE id IN ({placeholders})",
                ids,
            )
            count = cur.rowcount
            conn.commit()
            conn.close()
            subprocess.call(["pkill", "-USR1", "-x", "gdsyncd"])
            self.load_queue_from_db()
            self.notify("gdsync", f"Se reactivaron {count} tarea(s) para reintentar.")
        except Exception as e:
            QMessageBox.critical(self, "Error", f"No se pudieron reintentar las tareas: {e}")

    def discard_selected_tasks(self):
        selected_rows = self.table_queue.selectionModel().selectedRows()
        if not selected_rows:
            return
        ids = [int(self.table_queue.item(r.row(), 0).text()) for r in selected_rows]
        resp = QMessageBox.question(
            self,
            "Descartar tareas",
            f"¿Estás seguro de que querés descartar y eliminar {len(ids)} tarea(s) de la cola de sincronización?\n(Los archivos en tu equipo no se borrarán).",
            QMessageBox.StandardButton.Yes | QMessageBox.StandardButton.No,
            QMessageBox.StandardButton.No,
        )
        if resp != QMessageBox.StandardButton.Yes:
            return
        dbp = db_path()
        if not dbp.exists():
            return
        try:
            conn = sqlite3.connect(dbp)
            cur = conn.cursor()
            placeholders = ",".join("?" for _ in ids)
            cur.execute(f"DELETE FROM upload_queue WHERE id IN ({placeholders})", ids)
            count = cur.rowcount
            conn.commit()
            conn.close()
            self.load_queue_from_db()
            self.notify("gdsync", f"Se descartaron {count} tarea(s) de la cola.")
        except Exception as e:
            QMessageBox.critical(self, "Error", f"No se pudieron descartar las tareas: {e}")

    # ---- PESTAÑA SINCRONIZACIÓN SELECTIVA ----
    def sync_all_clicked(self):
        cli = find_cli()
        subprocess.call([cli, "folder", "reset"])
        self.cfg = load_config()
        self.load_remote_folders()
        QMessageBox.information(self, "Sincronización Selectiva", "Se restableció la sincronización a toda la unidad ('root').")
        self.refresh_status_background()

    def load_remote_folders(self):
        cli = find_cli()
        self.btn_refresh_folders.setEnabled(False)
        self.btn_refresh_folders.setText("Consultando carpetas en Google Drive...")

        self.folder_worker = FolderLoaderWorker(cli)
        self.folder_worker.finished_signal.connect(self.on_remote_folders_loaded)
        self.folder_worker.start()

    def on_remote_folders_loaded(self, success, out):
        self.btn_refresh_folders.setEnabled(True)
        self.btn_refresh_folders.setText("Cargar carpetas de Google Drive")

        if not success:
            QMessageBox.warning(self, "Error", f"No se pudo consultar carpetas remotas:\n{out}\n\nAsegurate de haber iniciado sesión con 'gdsync login'.")
            return

        self.tree_folders.blockSignals(True)
        self.tree_folders.clear()

        sync_all = self.cfg.get("sync_folders", ["root"]) == ["root"]
        active_ids = set(self.cfg.get("sync_folders", []))

        lines = out.strip().split("\n")
        in_table = False
        for line in lines:
            if line.startswith("ID"):
                in_table = True
                continue
            if line.startswith("---"):
                continue
            if in_table and line.strip() and not line.startswith("Total"):
                parts = line.split(maxsplit=2)
                if len(parts) >= 2:
                    fid = parts[0].strip()
                    fname = parts[1].strip()

                    item = QTreeWidgetItem([fname, fid])
                    item.setFlags(item.flags() | Qt.ItemFlag.ItemIsUserCheckable)
                    checked = sync_all or (fid in active_ids)
                    item.setCheckState(0, Qt.CheckState.Checked if checked else Qt.CheckState.Unchecked)
                    self.tree_folders.addTopLevelItem(item)

        self.tree_folders.blockSignals(False)

    def on_folder_checked(self, item, column):
        fid = item.text(1)
        checked = (item.checkState(0) == Qt.CheckState.Checked)
        sync_folders = self.cfg.get("sync_folders", ["root"])

        if sync_folders == ["root"]:
            sync_folders = []

        if checked and fid not in sync_folders:
            sync_folders.append(fid)
        elif not checked and fid in sync_folders:
            sync_folders.remove(fid)

        if not sync_folders:
            sync_folders = ["root"]

        self.cfg["sync_folders"] = sync_folders
        save_config(self.cfg)
        self.refresh_status_background()

    # ---- PESTAÑA CUENTA Y AUTENTICACIÓN ----
    def show_google_cloud_tutorial(self):
        dlg = QDialog(self)
        dlg.setWindowTitle("Guía: Configurar Google Cloud Console para gdsync")
        dlg.resize(780, 620)
        vbox = QVBoxLayout(dlg)

        browser = QTextBrowser()
        browser.setOpenExternalLinks(True)

        palette = self.palette()
        bg_lightness = palette.color(palette.ColorRole.Base).lightness()
        is_dark = bg_lightness < 128

        if is_dark:
            text_color = "#eff0f1"
            subtext_color = "#bdc3c7"
            link_color = "#3daee9"
            title_color = "#3daee9"
            step1_color = "#2ecc71"
            step2_color = "#f39c12"
            step3_color = "#3daee9"
            step4_color = "#bb8fce"
            warn_bg = "#3a2a14"
            warn_border = "#f39c12"
            warn_text = "#fad7a0"
            code_bg = "#31363b"
            code_color = "#f9e79f"
            hr_color = "#454b51"
        else:
            text_color = "#212529"
            subtext_color = "#495057"
            link_color = "#1a73e8"
            title_color = "#1a73e8"
            step1_color = "#2e7d32"
            step2_color = "#d84315"
            step3_color = "#1565c0"
            step4_color = "#6a1b9a"
            warn_bg = "#fff3e0"
            warn_border = "#ff9800"
            warn_text = "#212529"
            code_bg = "#f1f3f4"
            code_color = "#b02a37"
            hr_color = "#e0e0e0"

        html = f"""
        <div style='font-family: sans-serif; line-height: 1.6; color: {text_color};'>
          <h2 style='color: {title_color}; margin-top: 0; margin-bottom: 12px;'>Guía Completa: Configurar Google Cloud Console para gdsync</h2>
          <hr style='border: 0; border-top: 1px solid {hr_color}; margin: 12px 0 16px 0;'>

          <h3 style='color: {step1_color}; margin-bottom: 8px;'>Paso 1: Crear el proyecto y activar la API de Drive</h3>
          <ol style='color: {text_color}; padding-left: 24px; margin-top: 4px;'>
            <li>Ingresá a <a href='https://console.cloud.google.com/' style='color: {link_color}; font-weight: bold;'>Google Cloud Console</a> con tu cuenta de Google.</li>
            <li>Arriba a la izquierda, hacé clic en el selector de proyectos y seleccioná <b>«Proyecto nuevo»</b> (podés llamarlo <code style='background: {code_bg}; color: {code_color}; padding: 2px 6px; border-radius: 3px;'>gdsync</code>).</li>
            <li>En el menú de navegación (o en el buscador superior), andá a <b>APIs y servicios &rarr; Biblioteca</b>.</li>
            <li>Buscá <b>«Google Drive API»</b> y hacé clic en el botón <b>«Habilitar»</b>.</li>
          </ol>

          <h3 style='color: {step2_color}; margin-top: 20px; margin-bottom: 8px;'>Paso 2: Evitar que el token expire a los 7 días (Publicar la App)</h3>
          <div style='background: {warn_bg}; padding: 12px 16px; border-left: 4px solid {warn_border}; border-radius: 4px; margin: 8px 0 12px 0; color: {warn_text};'>
            <b>¿Por qué expira?</b> Por defecto, Google deja los proyectos nuevos en modo <i>«Prueba» (Testing)</i>, lo que causa que el token expire obligatoriamente a los 7 días (error <code>invalid_grant</code>). Pasándolo a producción no volverá a expirar jamás.
          </div>
          <ol style='color: {text_color}; padding-left: 24px; margin-top: 4px;'>
            <li>En el menú lateral, andá a <b>APIs y servicios &rarr; Pantalla de consentimiento de OAuth</b> (ahora llamada <b>Google Auth Platform</b>).</li>
            <li>Si te pide tipo de usuario, seleccioná <b>«Externo»</b> y completá el nombre de app (<code style='background: {code_bg}; color: {code_color}; padding: 2px 6px; border-radius: 3px;'>gdsync</code>) y tu correo de asistencia.</li>
            <li>En el menú lateral izquierdo de Google Auth Platform, andá a <b>«Información de la marca»</b>:
              <ul style='margin-top: 4px;'>
                <li>En <b>Página principal de la app</b>, ingresá tu sitio web o una URL válida (ej: <code style='background: {code_bg}; color: {code_color}; padding: 2px 6px; border-radius: 3px;'>https://tiago.ar</code> o <code style='background: {code_bg}; color: {code_color}; padding: 2px 6px; border-radius: 3px;'>https://google.com</code>).</li>
                <li>En <b>Vínculo a la Política de Privacidad</b>, ingresá una URL (ej: <code style='background: {code_bg}; color: {code_color}; padding: 2px 6px; border-radius: 3px;'>https://tiago.ar/privacy</code> o la misma URL).</li>
                <li>Hacé clic en <b>«Guardar»</b> al final de la página.</li>
              </ul>
            </li>
            <li style='margin-top: 6px;'>Ahora hacé clic en la pestaña <b>«Público»</b>:
              <ul style='margin-top: 4px;'>
                <li>Verás que el <b>Estado de publicación</b> dice <i>«Prueba»</i>.</li>
                <li>Hacé clic en el botón <b>«Publicar app»</b> y luego en <b>«Confirmar»</b>.</li>
                <li><b>Nota:</b> NO necesitás pagar ni enviar tu app a revisión oficial con Google. Al ser para tu uso personal, queda publicada y el acceso será permanente.</li>
              </ul>
            </li>
          </ol>

          <h3 style='color: {step3_color}; margin-top: 20px; margin-bottom: 8px;'>Paso 3: Crear las Credenciales (client_secret)</h3>
          <ol style='color: {text_color}; padding-left: 24px; margin-top: 4px;'>
            <li>En el menú lateral, andá a <b>APIs y servicios &rarr; Credenciales</b>.</li>
            <li>Hacé clic en <b>«+ CREAR CREDENCIALES» &rarr; «ID de cliente de OAuth»</b>.</li>
            <li>En <i>Tipo de aplicación</i>, elegí <b>«App de escritorio»</b> (Desktop app).</li>
            <li>De nombre ingresá <code style='background: {code_bg}; color: {code_color}; padding: 2px 6px; border-radius: 3px;'>gdsync-desktop</code> y presioná <b>«Crear»</b>.</li>
            <li>En la ventana emergente, hacé clic en <b>«Descargar JSON»</b> (se guardará un archivo como <code style='background: {code_bg}; color: {code_color}; padding: 2px 6px; border-radius: 3px;'>client_secret_*.json</code> en tu carpeta de Descargas).</li>
          </ol>

          <h3 style='color: {step4_color}; margin-top: 20px; margin-bottom: 8px;'>Paso 4: Conectar gdsync e Iniciar Sesión</h3>
          <ol style='color: {text_color}; padding-left: 24px; margin-top: 4px;'>
            <li>En esta pestaña <b>👤 Cuenta y Conexión</b> de gdsync, presioná el botón:<br>
            <b>«📄 Importar archivo JSON de Google (client_secret_*.json)...»</b> y elegí el archivo descargado.</li>
            <li>Hacé clic en el botón verde <b>«🔑 Iniciar Sesión con Google (OAuth 2.0)»</b>.</li>
            <li>Se abrirá tu navegador web:
              <ul style='margin-top: 4px;'>
                <li>Elegí tu cuenta de Google.</li>
                <li>Si aparece una advertencia de <i>«Google no verificó esta app»</i>, hacé clic en <b>«Configuración avanzada»</b> &rarr; <b>«Ir a gdsync (no seguro)»</b> &rarr; <b>«Continuar»</b>.</li>
              </ul>
            </li>
          </ol>

          <p style='margin-top: 18px; font-size: 15px; font-weight: bold; color: {step1_color};'>
            ¡Listo! Tu unidad de Google Drive quedará conectada de forma permanente y tus archivos se sincronizarán automáticamente.
          </p>
        </div>
        """
        browser.setHtml(html)
        vbox.addWidget(browser)

        btn_box = QHBoxLayout()
        btn_open_cloud = QPushButton("🌐 Abrir Google Cloud Console en el navegador")
        btn_open_cloud.setIcon(self.style().standardIcon(QStyle.StandardPixmap.SP_DesktopIcon))
        btn_open_cloud.clicked.connect(lambda: subprocess.Popen(["xdg-open", "https://console.cloud.google.com/"]))
        btn_box.addWidget(btn_open_cloud)

        btn_box.addStretch()

        btn_close = QPushButton("Entendido / Cerrar")
        btn_close.clicked.connect(dlg.accept)
        btn_box.addWidget(btn_close)

        vbox.addLayout(btn_box)
        dlg.exec()

    def toggle_secret_visibility(self):
        if self.txt_client_secret.echoMode() == QLineEdit.EchoMode.Password:
            self.txt_client_secret.setEchoMode(QLineEdit.EchoMode.Normal)
            self.btn_toggle_secret.setText("🔒")
        else:
            self.txt_client_secret.setEchoMode(QLineEdit.EchoMode.Password)
            self.btn_toggle_secret.setText("👁️")

    def import_credentials_json_dialog(self):
        initial_dir = str(Path.home() / "Descargas")
        if not os.path.exists(initial_dir):
            initial_dir = str(Path.home() / "Downloads")
        if not os.path.exists(initial_dir):
            initial_dir = str(Path.home())

        file_path, _ = QFileDialog.getOpenFileName(
            self,
            "Seleccionar archivo JSON de credenciales de Google",
            initial_dir,
            "Archivos JSON (*.json);;Todos los archivos (*)"
        )

        if not file_path:
            return

        try:
            cid, secret = parse_google_credentials_json(file_path)
            self.txt_client_id.setText(cid)
            self.txt_client_secret.setText(secret)

            self.cfg["client_id"] = cid
            self.cfg["client_secret"] = secret
            save_config(self.cfg)

            QMessageBox.information(
                self,
                "Credenciales importadas",
                f"¡Archivo JSON leído correctamente!\n\n"
                f"• Client ID: {cid[:25]}...\n"
                f"• Archivo: {Path(file_path).name}\n\n"
                f"Las claves se han guardado. Ya podés hacer clic en 'Iniciar Sesión con Google'."
            )
        except Exception as e:
            QMessageBox.critical(self, "Error al importar JSON", f"No se pudieron extraer las credenciales del archivo:\n{e}")

    def save_credentials_manual(self):
        cid = self.txt_client_id.text().strip()
        secret = self.txt_client_secret.text().strip()
        if not cid or not secret:
            QMessageBox.warning(self, "Credenciales vacías", "Debés ingresar tanto el Client ID como el Client Secret.")
            return
        self.cfg["client_id"] = cid
        self.cfg["client_secret"] = secret
        save_config(self.cfg)
        QMessageBox.information(self, "Guardado", "Credenciales guardadas correctamente.")

    def start_login_flow(self):
        cid = self.txt_client_id.text().strip()
        secret = self.txt_client_secret.text().strip()
        if not cid or not secret:
            QMessageBox.warning(self, "Credenciales faltantes", "Primero importá el archivo JSON de Google o completá el Client ID y Client Secret.")
            return

        self.btn_login.setEnabled(False)
        self.btn_login.setText("Esperando autorización en el navegador...")

        cli = find_cli()
        self.login_worker = LoginWorker(cli, client_id=cid, client_secret=secret)
        self.login_worker.finished_signal.connect(self.on_login_finished)
        self.login_worker.start()

    def on_login_finished(self, success, msg):
        self.btn_login.setEnabled(True)
        self.btn_login.setText("🔑 Iniciar Sesión con Google (OAuth 2.0)")
        if success:
            QMessageBox.information(self, "Autenticación exitosa", msg)
        else:
            QMessageBox.warning(self, "Fallo de autenticación", msg)
        self.refresh_all()

    def logout_account(self):
        reply = QMessageBox.question(
            self,
            "Cerrar sesión",
            "¿Estás seguro de que querés cerrar la sesión? Se revocará el acceso en Google y deberás volver a iniciar sesión para montar el disco.",
            QMessageBox.StandardButton.Yes | QMessageBox.StandardButton.No
        )
        if reply == QMessageBox.StandardButton.Yes:
            cli = find_cli()
            subprocess.call([cli, "logout"])
            QMessageBox.information(self, "Sesión cerrada", "La sesión se ha cerrado y el token fue revocado.")
            self.refresh_all()

    # ---- PESTAÑA AJUSTES ----
    def browse_mount(self):
        dir_selected = QFileDialog.getExistingDirectory(self, "Seleccionar carpeta de montaje", self.txt_mount.text())
        if dir_selected:
            self.txt_mount.setText(dir_selected)

    def on_autostart_gui_toggled(self, checked: bool):
        set_gui_autostart(checked)

    def on_autostart_daemon_toggled(self, checked: bool):
        set_daemon_autostart(checked)

    def on_notifications_toggled(self, checked: bool):
        self.cfg["notifications_enabled"] = checked
        save_config(self.cfg)
        if not checked:
            self.close_queue_notification()
        else:
            self.refresh_status_background()

    def on_persistent_notify_toggled(self, checked: bool):
        self.cfg["persistent_upload_notify"] = checked
        save_config(self.cfg)
        if not checked:
            self.close_queue_notification()
        else:
            self.refresh_status_background()

    def on_close_to_tray_toggled(self, checked: bool):
        self.cfg["close_to_tray"] = checked
        save_config(self.cfg)

    def save_settings(self):
        mount = self.txt_mount.text().strip()
        if not mount:
            QMessageBox.warning(self, "Ajustes", "La ruta de montaje no puede estar vacía.")
            return
        self.cfg["mount_point"] = mount
        self.cfg["cache_max_gb"] = self.spin_cache.value()
        self.cfg["poll_interval_secs"] = self.spin_poll.value()
        self.cfg["auto_retry_secs"] = self.spin_auto_retry.value() * 60
        self.cfg["notifications_enabled"] = self.chk_notifications.isChecked()
        self.cfg["persistent_upload_notify"] = self.chk_persistent_notify.isChecked()
        self.cfg["close_to_tray"] = self.chk_close_to_tray.isChecked()
        save_config(self.cfg)
        set_gui_autostart(self.chk_autostart_gui.isChecked())
        set_daemon_autostart(self.chk_autostart_daemon.isChecked())
        ensure_dolphin_bookmark(mount)
        QMessageBox.information(
            self,
            "Ajustes",
            "Configuración guardada correctamente.\n\n"
            "Nota: El daemon detectará automáticamente los cambios de sondeo y carpetas en su siguiente ciclo."
        )
        self.refresh_status_background()

    def action_resync(self):
        reply = QMessageBox.warning(
            self,
            "Restablecer caché y base de datos (Resync)",
            "Esta acción borrará la base de datos local y la caché de archivos en disco. "
            "El servicio se detendrá de forma segura y tus datos se descargarán de nuevo bajo demanda.\n\n"
            "¿Deseás continuar?",
            QMessageBox.StandardButton.Yes | QMessageBox.StandardButton.No
        )
        if reply == QMessageBox.StandardButton.Yes:
            if is_daemon_running():
                subprocess.call(["pkill", "-f", "gdsyncd"])
                # Esperar hasta 5 segundos a que el daemon termine de desmontar
                for _ in range(50):
                    if not is_daemon_running():
                        break
                    time.sleep(0.1)
                mount = self.cfg.get("mount_point", str(Path.home() / "GoogleDrive"))
                subprocess.call(["fusermount3", "-u", mount], stderr=subprocess.DEVNULL)

            cli = find_cli()
            subprocess.call([cli, "resync"])
            QMessageBox.information(self, "Resync", "Caché y base de datos local borradas correctamente.")
            self.refresh_all()

    # ---- PESTAÑA LOGS ----
    def load_system_logs(self):
        lines = []
        try:
            out = subprocess.check_output(
                ["journalctl", "--user", "-u", "gdsyncd", "-n", "80", "--no-pager"],
                stderr=subprocess.DEVNULL,
                text=True
            )
            lines = out.strip().split("\n")
        except Exception:
            pass

        if not lines or (len(lines) == 1 and not lines[0]):
            lines = ["(No se encontraron registros en journalctl para gdsyncd. Si ejecutás el daemon en terminal, podés ver los logs directamente en stdout)."]

        self.txt_logs.setPlainText("\n".join(lines))
        self.txt_logs.verticalScrollBar().setValue(self.txt_logs.verticalScrollBar().maximum())

    # ---- ACCIONES GLOBALES ----
    def toggle_daemon(self):
        if is_daemon_running():
            subprocess.call(["pkill", "-f", "gdsyncd"])
        else:
            d = find_daemon()
            subprocess.Popen([d])
        QTimer.singleShot(1000, self.refresh_all)

    def restart_daemon(self):
        if is_daemon_running():
            subprocess.call(["pkill", "-f", "gdsyncd"])
            for _ in range(30):
                if not is_daemon_running():
                    break
                time.sleep(0.1)
            mount = self.cfg.get("mount_point", str(Path.home() / "GoogleDrive"))
            subprocess.call(["fusermount3", "-u", mount], stderr=subprocess.DEVNULL)
            QTimer.singleShot(500, lambda: subprocess.Popen([find_daemon()]))
        else:
            subprocess.Popen([find_daemon()])
        QTimer.singleShot(1500, self.refresh_all)

    def open_in_dolphin(self):
        mount = self.cfg.get("mount_point", str(Path.home() / "GoogleDrive"))
        os.makedirs(mount, exist_ok=True)
        ensure_dolphin_bookmark(mount)
        subprocess.Popen(["xdg-open", mount])

    def open_drive_web_root(self):
        subprocess.Popen(["xdg-open", "https://drive.google.com"])

    def trigger_poll_now(self):
        if is_daemon_running():
            subprocess.call(["pkill", "-USR1", "-x", "gdsyncd"])
            self.notify("gdsync", "Comprobando cambios remotos en Google Drive...")
            QTimer.singleShot(1500, self.refresh_all)
        else:
            QMessageBox.warning(
                self,
                "Servicio detenido",
                "El servicio gdsyncd no está en ejecución. Inicialo primero para comprobar cambios remotos."
            )
            self.refresh_all()


def main():
    try:
        import ctypes
        libc = ctypes.CDLL("libc.so.6")
        libc.prctl(15, b"gdsync\0", 0, 0, 0)
    except Exception:
        pass

    app = QApplication(sys.argv)
    app.setApplicationName("gdsync")
    app.setApplicationDisplayName("gdsync")
    app.setDesktopFileName("gdsync")
    app.setWindowIcon(load_app_icon())
    app.setQuitOnLastWindowClosed(False)

    SOCKET_NAME = "gdsync_gui_ipc"

    # Intentar comunicar con una instancia previa
    socket = QLocalSocket()
    socket.connectToServer(SOCKET_NAME)
    if socket.waitForConnected(300):
        # Si ya hay una instancia y no pedimos inicio minimizado, pedirle que muestre la ventana
        if "--minimized" not in sys.argv and "--tray" not in sys.argv:
            socket.write(b"SHOW\n")
            socket.waitForBytesWritten(300)
        print("gdsync gui ya se encuentra en ejecución.")
        sys.exit(0)

    # Si no se conectó, somos la instancia principal
    server = QLocalServer()
    QLocalServer.removeServer(SOCKET_NAME)
    server.listen(SOCKET_NAME)

    win = GdsyncWindow()

    def handle_ipc():
        conn = server.nextPendingConnection()
        if conn:
            def on_ready():
                try:
                    data = conn.readAll().data().decode("utf-8")
                    if "SHOW" in data:
                        win.show_and_raise()
                except Exception:
                    pass
            conn.readyRead.connect(on_ready)

    server.newConnection.connect(handle_ipc)

    start_minimized = "--minimized" in sys.argv or "--tray" in sys.argv
    if not start_minimized:
        win.show()
    sys.exit(app.exec())

if __name__ == "__main__":
    main()
