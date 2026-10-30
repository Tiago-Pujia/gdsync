#!/usr/bin/env bash
set -e

PREFIX="${PREFIX:-$HOME/.local}"
BIN_DIR="$PREFIX/bin"
SHARE_DIR="$PREFIX/share"
APPLICATIONS_DIR="$SHARE_DIR/applications"
SERVICEMENUS_DIR="$SHARE_DIR/kio/servicemenus"
SYSTEMD_USER_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
GDSYNC_DATA_DIR="$SHARE_DIR/gdsync"

echo "=== Desinstalando gdsync ==="

# Detener servicio si estuviera activo
systemctl --user stop gdsyncd 2>/dev/null || true
systemctl --user disable gdsyncd 2>/dev/null || true

# Eliminar binarios y accesos
rm -f "$BIN_DIR/gdsync" "$BIN_DIR/gdsyncd" "$BIN_DIR/gdsync-gui"
rm -f "$APPLICATIONS_DIR/gdsync.desktop"
rm -f "$SERVICEMENUS_DIR/gdsync.desktop"
rm -f "$SYSTEMD_USER_DIR/gdsyncd.service"
rm -f "${XDG_CONFIG_HOME:-$HOME/.config}/autostart/gdsync-gui.desktop"
rm -rf "$GDSYNC_DATA_DIR"

if command -v kbuildsycoca6 >/dev/null 2>&1; then
    kbuildsycoca6 --noincremental >/dev/null 2>&1 || true
elif command -v kbuildsycoca5 >/dev/null 2>&1; then
    kbuildsycoca5 --noincremental >/dev/null 2>&1 || true
fi

if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$APPLICATIONS_DIR" >/dev/null 2>&1 || true
fi

echo "=== Desinstalación completada ==="
echo "Nota: Tu configuración (~/.config/gdsync) y caché (~/.cache/gdsync) se conservaron intactas."
echo "Para borrarlas completamente ejecutá: rm -rf ~/.config/gdsync ~/.cache/gdsync ~/.local/share/gdsync"
