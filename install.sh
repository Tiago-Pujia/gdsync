#!/usr/bin/env bash
set -e

# Script de instalación de gdsync para Linux (KDE Plasma / GNOME / etc.)

# Detect real user if invoked via sudo
if [ -n "$SUDO_USER" ] && [ "$SUDO_USER" != "root" ]; then
    REAL_USER="$SUDO_USER"
    REAL_HOME=$(getent passwd "$SUDO_USER" | cut -d: -f6)
else
    REAL_USER="$USER"
    REAL_HOME="$HOME"
fi

# Ensure Cargo is on PATH even when invoked with sudo
if ! command -v cargo >/dev/null 2>&1; then
    if [ -x "$REAL_HOME/.cargo/bin/cargo" ]; then
        export PATH="$REAL_HOME/.cargo/bin:$PATH"
    elif [ -x "$HOME/.cargo/bin/cargo" ]; then
        export PATH="$HOME/.cargo/bin:$PATH"
    fi
fi

if [ -z "$PREFIX" ]; then
    if [ "$EUID" -eq 0 ]; then
        PREFIX="/usr/local"
    else
        PREFIX="$HOME/.local"
    fi
fi

BIN_DIR="$PREFIX/bin"
SHARE_DIR="$PREFIX/share"
APPLICATIONS_DIR="$SHARE_DIR/applications"
SERVICEMENUS_DIR="$SHARE_DIR/kio/servicemenus"
SYSTEMD_USER_DIR="$REAL_HOME/.config/systemd/user"
GDSYNC_DATA_DIR="$SHARE_DIR/gdsync"

echo "=== Instalando gdsync ==="
echo "Usuario destino: $REAL_USER"
echo "Destino de binarios: $BIN_DIR"

# 1. Determinar fuente de binarios (paquete precompilado o compilación local con Cargo)
if [ -f "bin/gdsync" ] && [ -f "bin/gdsyncd" ] && [ -f "bin/gdsync-gui" ]; then
    SRC_GDSYNC="bin/gdsync"
    SRC_GDSYNCD="bin/gdsyncd"
    SRC_GDSYNC_GUI="bin/gdsync-gui"
else
    echo "Compilando binarios con Cargo (workspace completo)..."
    if [ "$EUID" -eq 0 ] && [ -n "$SUDO_USER" ] && [ "$SUDO_USER" != "root" ]; then
        sudo -u "$SUDO_USER" PATH="$PATH" cargo build --workspace --release
    else
        cargo build --workspace --release
    fi
    SRC_GDSYNC="target/release/gdsync"
    SRC_GDSYNCD="target/release/gdsyncd"
    SRC_GDSYNC_GUI="target/release/gdsync-gui"
fi

# 2. Crear carpetas de destino
mkdir -p "$BIN_DIR"
mkdir -p "$APPLICATIONS_DIR"
mkdir -p "$SERVICEMENUS_DIR"
mkdir -p "$SYSTEMD_USER_DIR"
mkdir -p "$GDSYNC_DATA_DIR"

# 3. Instalar binarios nativos en Rust
echo "Instalando binarios nativos en $BIN_DIR..."
cp --remove-destination "$SRC_GDSYNC" "$BIN_DIR/gdsync"
cp --remove-destination "$SRC_GDSYNCD" "$BIN_DIR/gdsyncd"
cp --remove-destination "$SRC_GDSYNC_GUI" "$BIN_DIR/gdsync-gui"
chmod +x "$BIN_DIR/gdsync" "$BIN_DIR/gdsyncd" "$BIN_DIR/gdsync-gui"

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
sed "s|^ExecStart=.*|ExecStart=$BIN_DIR/gdsyncd|g" packaging/systemd/gdsyncd.service > "$SYSTEMD_USER_DIR/gdsyncd.service"
if [ "$EUID" -eq 0 ] && [ -n "$SUDO_USER" ] && [ "$SUDO_USER" != "root" ]; then
    chown "$REAL_USER:$REAL_USER" "$SYSTEMD_USER_DIR/gdsyncd.service"
fi

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
