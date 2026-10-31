#!/usr/bin/env bash
set -e

# Script de instalación de gdsync para Linux (KDE Plasma / GNOME / etc.)

PREFIX="${PREFIX:-$HOME/.local}"
BIN_DIR="$PREFIX/bin"
SHARE_DIR="$PREFIX/share"
APPLICATIONS_DIR="$SHARE_DIR/applications"
SERVICEMENUS_DIR="$SHARE_DIR/kio/servicemenus"
SYSTEMD_USER_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
GDSYNC_DATA_DIR="$SHARE_DIR/gdsync"

echo "=== Instalando gdsync ==="
echo "Destino de binarios: $BIN_DIR"

# 1. Determinar fuente de binarios (paquete precompilado o compilación local con Cargo)
if [ -f "bin/gdsync" ] && [ -f "bin/gdsyncd" ]; then
    SRC_GDSYNC="bin/gdsync"
    SRC_GDSYNCD="bin/gdsyncd"
elif [ -f "target/release/gdsync" ] && [ -f "target/release/gdsyncd" ]; then
    SRC_GDSYNC="target/release/gdsync"
    SRC_GDSYNCD="target/release/gdsyncd"
else
    echo "Compilando binarios con Cargo..."
    cargo build --workspace --release
    SRC_GDSYNC="target/release/gdsync"
    SRC_GDSYNCD="target/release/gdsyncd"
fi

# 2. Crear carpetas de destino
mkdir -p "$BIN_DIR"
mkdir -p "$APPLICATIONS_DIR"
mkdir -p "$SERVICEMENUS_DIR"
mkdir -p "$SYSTEMD_USER_DIR"
mkdir -p "$GDSYNC_DATA_DIR/gui"

# 3. Instalar binarios
echo "Instalando binarios en $BIN_DIR..."
cp --remove-destination "$SRC_GDSYNC" "$BIN_DIR/gdsync-bin"
cp --remove-destination "$SRC_GDSYNCD" "$BIN_DIR/gdsyncd"
chmod +x "$BIN_DIR/gdsync-bin" "$BIN_DIR/gdsyncd"

# 4. Instalar acciones, GUI y crear wrappers ejecutables
echo "Instalando interfaz gráfica y acciones auxiliares..."
cp --remove-destination src/actions.py "$GDSYNC_DATA_DIR/actions.py"
chmod +x "$GDSYNC_DATA_DIR/actions.py"
cp --remove-destination src/gui/main.py "$GDSYNC_DATA_DIR/gui/main.py"
chmod +x "$GDSYNC_DATA_DIR/gui/main.py"

cat << 'EOF' > "$BIN_DIR/gdsync"
#!/bin/sh
GDSYNC_DATA="${XDG_DATA_HOME:-$HOME/.local/share}/gdsync"

if [ "$1" = "gui" ]; then
    shift
    exec python3 "$GDSYNC_DATA/gui/main.py" "$@"
fi

if [ "$1" = "pin" ] || [ "$1" = "unpin" ] || [ "$1" = "pinned" ] || [ "$1" = "offline" ] || [ "$1" = "copy-link" ] || [ "$1" = "open-web" ]; then
    if [ -f "$GDSYNC_DATA/actions.py" ]; then
        exec python3 "$GDSYNC_DATA/actions.py" "$@"
    fi
fi

exec "$HOME/.local/bin/gdsync-bin" "$@"
EOF
chmod +x "$BIN_DIR/gdsync"

cat << 'EOF' > "$BIN_DIR/gdsync-gui"
#!/bin/sh
exec python3 "${XDG_DATA_HOME:-$HOME/.local/share}/gdsync/gui/main.py" "$@"
EOF
chmod +x "$BIN_DIR/gdsync-gui"

# 5. Instalar iconos de la aplicación
echo "Instalando iconografía de gdsync..."
mkdir -p "$SHARE_DIR/icons/hicolor/scalable/apps"
mkdir -p "$SHARE_DIR/icons/hicolor/48x48/apps"
mkdir -p "$SHARE_DIR/icons/hicolor/128x128/apps"
mkdir -p "$SHARE_DIR/icons/hicolor/256x256/apps"
cp packaging/icons/hicolor/scalable/apps/gdsync.svg "$SHARE_DIR/icons/hicolor/scalable/apps/gdsync.svg"
cp packaging/icons/hicolor/48x48/apps/gdsync.png "$SHARE_DIR/icons/hicolor/48x48/apps/gdsync.png"
cp packaging/icons/hicolor/128x128/apps/gdsync.png "$SHARE_DIR/icons/hicolor/128x128/apps/gdsync.png"
cp packaging/icons/hicolor/256x256/apps/gdsync.png "$SHARE_DIR/icons/hicolor/256x256/apps/gdsync.png"
cp packaging/icons/hicolor/256x256/apps/gdsync.png "$GDSYNC_DATA_DIR/icon.png"
cp packaging/icons/hicolor/scalable/apps/gdsync.svg "$GDSYNC_DATA_DIR/icon.svg"

# 6. Instalar integración con Dolphin / Gestores de archivos
echo "Instalando ServiceMenu para Dolphin..."
cp packaging/dolphin/gdsync.desktop "$SERVICEMENUS_DIR/gdsync.desktop"
chmod +x "$SERVICEMENUS_DIR/gdsync.desktop"

# 7. Instalar lanzador de escritorio
echo "Instalando entrada de aplicación de escritorio..."
cp packaging/desktop/gdsync.desktop "$APPLICATIONS_DIR/gdsync.desktop"
chmod +x "$APPLICATIONS_DIR/gdsync.desktop"

# 8. Instalar servicio systemd de usuario
echo "Instalando servicio systemd de usuario..."
sed "s|%h/.cargo/bin/gdsyncd|$BIN_DIR/gdsyncd|g" packaging/systemd/gdsyncd.service > "$SYSTEMD_USER_DIR/gdsyncd.service"

# 9. Refrescar cachés de KDE / XDG si las utilidades existen
if command -v kbuildsycoca6 >/dev/null 2>&1; then
    kbuildsycoca6 --noincremental >/dev/null 2>&1 || true
elif command -v kbuildsycoca5 >/dev/null 2>&1; then
    kbuildsycoca5 --noincremental >/dev/null 2>&1 || true
fi

if command -v gtk-update-icon-cache >/dev/null 2>&1; then
    gtk-update-icon-cache -f -t "$SHARE_DIR/icons/hicolor" >/dev/null 2>&1 || true
fi

if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$APPLICATIONS_DIR" >/dev/null 2>&1 || true
fi

echo ""
echo "=== ¡Instalación completada con éxito! ==="
echo ""
echo "Próximos pasos recomendados:"
echo " 1. Asegurate de que $BIN_DIR esté en tu \$PATH (si aún no lo está)."
echo " 2. Iniciar sesión:            gdsync login"
echo " 3. Abrir la interfaz gráfica: gdsync gui (o buscala en el menú de aplicaciones)"
echo " 4. Iniciar el servicio en segundo plano:"
echo "    systemctl --user daemon-reload"
echo "    systemctl --user enable --now gdsyncd"
echo ""
