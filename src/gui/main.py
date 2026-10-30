#!/usr/bin/env python3
"""
gdsync-gui: Interfaz gráfica oficial para gdsync en KDE / Linux.
Construida con PyQt6 para integrarse de forma 100% nativa con KDE Plasma.
"""

import sys
import os
import json
import sqlite3
import subprocess
from pathlib import Path

from PyQt6.QtCore import Qt, QTimer, QSize
from PyQt6.QtGui import QIcon, QFont, QAction
from PyQt6.QtWidgets import (
    QApplication, QMainWindow, QWidget, QVBoxLayout, QHBoxLayout,
    QTabWidget, QLabel, QPushButton, QProgressBar, QTreeWidget,
    QTreeWidgetItem, QFileDialog, QSpinBox, QLineEdit, QMessageBox,
    QSystemTrayIcon, QMenu, QGroupBox, QStyle
)

def config_path():
    return Path.home() / ".config" / "gdsync" / "config.json"

def token_path():
    return Path.home() / ".config" / "gdsync" / "token.json"

def db_path():
    return Path.home() / ".local" / "share" / "gdsync" / "state.db"

def cache_dir():
    return Path.home() / ".cache" / "gdsync" / "blobs"

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
        "sync_folders": ["root"]
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


class GdsyncWindow(QMainWindow):
    def __init__(self):
        super().__init__()
        self.setWindowTitle("gdsync - Google Drive")
        self.resize(720, 540)
        self.cfg = load_config()

        # Widget central con pestañas
        central = QWidget()
        self.setCentralWidget(central)
        main_layout = QVBoxLayout(central)

        self.tabs = QTabWidget()
        main_layout.addWidget(self.tabs)

        self.init_tab_status()
        self.init_tab_selective()
        self.init_tab_settings()
        self.init_tray()

        # Timer para refrescar estado en vivo cada 3 segundos
        self.timer = QTimer(self)
        self.timer.timeout.connect(self.refresh_status)
        self.timer.start(3000)

        self.refresh_status()

    # ---- PESTAÑA 1: ESTADO Y MONITOR ----
    def init_tab_status(self):
        w = QWidget()
        layout = QVBoxLayout(w)

        # Grupo: Estado del Servicio
        grp_daemon = QGroupBox("Estado del servicio (gdsyncd)")
        daemon_lay = QHBoxLayout(grp_daemon)
        self.lbl_daemon = QLabel("Verificando...")
        self.btn_daemon_toggle = QPushButton("Iniciar servicio")
        self.btn_daemon_toggle.clicked.connect(self.toggle_daemon)
        self.btn_open_mount = QPushButton("Abrir en Dolphin")
        self.btn_open_mount.setIcon(self.style().standardIcon(QStyle.StandardPixmap.SP_DirIcon))
        self.btn_open_mount.clicked.connect(self.open_in_dolphin)

        daemon_lay.addWidget(self.lbl_daemon, stretch=1)
        daemon_lay.addWidget(self.btn_daemon_toggle)
        daemon_lay.addWidget(self.btn_open_mount)
        layout.addWidget(grp_daemon)

        # Grupo: Cuotas y Almacenamiento
        grp_storage = QGroupBox("Almacenamiento y Caché local")
        storage_lay = QVBoxLayout(grp_storage)

        self.lbl_cloud_storage = QLabel("Almacenamiento en Google Drive:")
        self.bar_cloud = QProgressBar()
        self.bar_cloud.setValue(0)

        self.lbl_local_cache = QLabel("Caché en disco:")
        self.bar_cache = QProgressBar()
        self.bar_cache.setValue(0)

        storage_lay.addWidget(self.lbl_cloud_storage)
        storage_lay.addWidget(self.bar_cloud)
        storage_lay.addSpacing(8)
        storage_lay.addWidget(self.lbl_local_cache)
        storage_lay.addWidget(self.bar_cache)
        layout.addWidget(grp_storage)

        # Grupo: Actividad de Sincronización
        grp_sync = QGroupBox("Detalles de sincronización")
        sync_lay = QVBoxLayout(grp_sync)
        self.lbl_items_count = QLabel("Elementos registrados: -")
        self.lbl_upload_queue = QLabel("Tareas pendientes de subida: 0")
        self.lbl_sync_mode = QLabel("Modo de sincronización: -")

        sync_lay.addWidget(self.lbl_items_count)
        sync_lay.addWidget(self.lbl_upload_queue)
        sync_lay.addWidget(self.lbl_sync_mode)
        layout.addWidget(grp_sync)

        layout.addStretch()
        self.tabs.addTab(w, "Estado")

    # ---- PESTAÑA 2: SINCRONIZACIÓN SELECTIVA ----
    def init_tab_selective(self):
        w = QWidget()
        layout = QVBoxLayout(w)

        lbl = QLabel("Seleccioná qué carpetas querés sincronizar en tu equipo local:")
        lbl.setWordWrap(True)
        layout.addWidget(lbl)

        self.tree_folders = QTreeWidget()
        self.tree_folders.setHeaderLabels(["Carpeta", "ID"])
        self.tree_folders.setColumnWidth(0, 360)
        self.tree_folders.itemChanged.connect(self.on_folder_checked)
        layout.addWidget(self.tree_folders)

        btn_box = QHBoxLayout()
        self.btn_refresh_folders = QPushButton("Cargar carpetas remotas")
        self.btn_refresh_folders.clicked.connect(self.load_remote_folders)
        self.btn_sync_all = QPushButton("Sincronizar toda la unidad (root)")
        self.btn_sync_all.clicked.connect(self.sync_all_clicked)

        btn_box.addWidget(self.btn_refresh_folders)
        btn_box.addWidget(self.btn_sync_all)
        btn_box.addStretch()
        layout.addLayout(btn_box)

        self.tabs.addTab(w, "Sincronización Selectiva")

    # ---- PESTAÑA 3: AJUSTES ----
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

        layout.addWidget(grp_opts)

        # Botón guardar
        btn_save = QPushButton("Guardar configuración")
        btn_save.clicked.connect(self.save_settings)
        layout.addWidget(btn_save)

        layout.addStretch()
        self.tabs.addTab(w, "Ajustes")

    # ---- BANDEJA DEL SISTEMA (TRAY) ----
    def init_tray(self):
        self.tray = QSystemTrayIcon(self)
        self.tray.setIcon(self.style().standardIcon(QStyle.StandardPixmap.SP_DriveHDIcon))
        self.tray.setToolTip("gdsync - Google Drive")

        menu = QMenu()
        act_open_dolphin = QAction("Abrir en Dolphin", self)
        act_open_dolphin.triggered.connect(self.open_in_dolphin)
        menu.addAction(act_open_dolphin)

        act_show = QAction("Abrir panel de control", self)
        act_show.triggered.connect(self.show_and_raise)
        menu.addAction(act_show)

        menu.addSeparator()
        act_quit = QAction("Salir", self)
        act_quit.triggered.connect(QApplication.instance().quit)
        menu.addAction(act_quit)

        self.tray.setContextMenu(menu)
        self.tray.activated.connect(self.on_tray_activated)
        self.tray.show()

    def on_tray_activated(self, reason):
        if reason == QSystemTrayIcon.ActivationReason.Trigger:
            self.show_and_raise()

    def show_and_raise(self):
        self.show()
        self.raise_()
        self.activateWindow()

    def closeEvent(self, event):
        # Al cerrar la ventana, minimizar a la bandeja en lugar de cerrar el proceso
        if self.tray.isVisible():
            self.hide()
            event.ignore()
        else:
            event.accept()

    # ---- LÓGICA DE ESTADO ----
    def refresh_status(self):
        running = is_daemon_running()
        if running:
            self.lbl_daemon.setText("🟢 <b>En ejecución</b> (Disco montado)")
            self.btn_daemon_toggle.setText("Detener servicio")
        else:
            self.lbl_daemon.setText("⚪ <b>Detenido</b>")
            self.btn_daemon_toggle.setText("Iniciar servicio")

        # Caché en disco
        cache_bytes = 0
        cdir = cache_dir()
        if cdir.exists():
            for f in cdir.glob("*"):
                if f.is_file():
                    cache_bytes += f.stat().st_size
        cache_mb = cache_bytes / (1024 * 1024)
        max_cache_mb = self.cfg.get("cache_max_gb", 10) * 1024
        self.lbl_local_cache.setText(f"Caché en disco: <b>{cache_mb:.1f} MB</b> de {self.cfg.get('cache_max_gb', 10)} GB configurados")
        pct_cache = min(100, int((cache_mb / max_cache_mb) * 100)) if max_cache_mb else 0
        self.bar_cache.setValue(pct_cache)

        # Base de datos local
        dbp = db_path()
        if dbp.exists():
            try:
                conn = sqlite3.connect(f"file:{dbp}?mode=ro", uri=True)
                cur = conn.cursor()
                cur.execute("SELECT COUNT(*) FROM items")
                items_cnt = cur.fetchone()[0]
                self.lbl_items_count.setText(f"Elementos registrados: <b>{items_cnt}</b>")

                cur.execute("SELECT COUNT(*) FROM upload_queue WHERE status='pending'")
                pending = cur.fetchone()[0]
                self.lbl_upload_queue.setText(f"Tareas pendientes de subida: <b>{pending}</b>")
                conn.close()
            except Exception:
                pass

        # Modo sincronización
        sync_folders = self.cfg.get("sync_folders", ["root"])
        if sync_folders == ["root"] or not sync_folders:
            self.lbl_sync_mode.setText("Modo de sincronización: <b>Toda la unidad (completo)</b>")
        else:
            self.lbl_sync_mode.setText(f"Modo de sincronización: <b>{len(sync_folders)} carpeta(s) selectiva(s)</b>")

    def toggle_daemon(self):
        if is_daemon_running():
            subprocess.call(["pkill", "-f", "gdsyncd"])
        else:
            cli_bin = Path(__file__).resolve().parent.parent.parent / "target" / "debug" / "gdsyncd"
            if not cli_bin.exists():
                cli_bin = "gdsyncd"
            subprocess.Popen([str(cli_bin)])
        self.refresh_status()

    def open_in_dolphin(self):
        mount = self.cfg.get("mount_point", str(Path.home() / "GoogleDrive"))
        subprocess.Popen(["xdg-open", mount])

    def browse_mount(self):
        dir_selected = QFileDialog.getExistingDirectory(self, "Seleccionar carpeta de montaje", self.txt_mount.text())
        if dir_selected:
            self.txt_mount.setText(dir_selected)

    def save_settings(self):
        self.cfg["mount_point"] = self.txt_mount.text().strip()
        self.cfg["cache_max_gb"] = self.spin_cache.value()
        self.cfg["poll_interval_secs"] = self.spin_poll.value()
        save_config(self.cfg)
        QMessageBox.information(self, "Ajustes", "Configuración guardada correctamente.")
        self.refresh_status()

    def sync_all_clicked(self):
        self.cfg["sync_folders"] = ["root"]
        save_config(self.cfg)
        self.load_remote_folders()
        QMessageBox.information(self, "Sincronización Selectiva", "Se restableció la sincronización a toda la unidad ('root').")

    def load_remote_folders(self):
        cli = Path(__file__).resolve().parent.parent.parent / "target" / "debug" / "gdsync"
        if not cli.exists():
            cli = "gdsync"
        try:
            out = subprocess.check_output([str(cli), "folder", "list-remote"], text=True)
        except Exception as e:
            QMessageBox.warning(self, "Error", f"No se pudo consultar carpetas remotas:\n{e}\n\nAsegurate de haber iniciado sesión con 'gdsync login'.")
            return

        self.tree_folders.blockSignals(True)
        self.tree_folders.clear()

        sync_all = self.cfg.get("sync_folders", ["root"]) == ["root"]
        active_ids = set(self.cfg.get("sync_folders", []))

        # Parsear las líneas de salida de list-remote
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
        self.refresh_status()


def main():
    app = QApplication(sys.argv)
    app.setQuitOnLastWindowClosed(False)
    win = GdsyncWindow()
    win.show()
    sys.exit(app.exec())

if __name__ == "__main__":
    main()
