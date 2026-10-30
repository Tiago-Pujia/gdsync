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

# 1. Compilar release si no existen los binarios
if [ ! -f "target/release/gdsync" ] || [ ! -f "target/release/gdsyncd" ]; then
    echo "Compilando binarios con Cargo..."
    cargo build --workspace --release
fi

# 2. Crear carpetas de destino
mkdir -p "$BIN_DIR"
mkdir -p "$APPLICATIONS_DIR"
mkdir -p "$SERVICEMENUS_DIR"
mkdir -p "$SYSTEMD_USER_DIR"
mkdir -p "$GDSYNC_DATA_DIR/gui"

# 3. Instalar binarios
echo "Instalando binarios en $BIN_DIR..."
cp --remove-destination target/release/gdsync "$BIN_DIR/gdsync"
cp --remove-destination target/release/gdsyncd "$BIN_DIR/gdsyncd"
chmod +x "$BIN_DIR/gdsync" "$BIN_DIR/gdsyncd"

# 4. Instalar GUI y crear launcher ejecutable
echo "Instalando interfaz gráfica (PyQt6)..."
cp --remove-destination src/gui/main.py "$GDSYNC_DATA_DIR/gui/main.py"
chmod +x "$GDSYNC_DATA_DIR/gui/main.py"

cat << 'EOF' > "$BIN_DIR/gdsync-gui"
#!/bin/sh
exec gdsync gui "$@"
EOF
chmod +x "$BIN_DIR/gdsync-gui"

# 5. Instalar integración con Dolphin / Gestores de archivos
echo "Instalando ServiceMenu para Dolphin..."
cp packaging/dolphin/gdsync.desktop "$SERVICEMENUS_DIR/gdsync.desktop"

# 6. Instalar lanzador de escritorio
echo "Instalando entrada de aplicación de escritorio..."
cp packaging/desktop/gdsync.desktop "$APPLICATIONS_DIR/gdsync.desktop"

# 7. Instalar servicio systemd de usuario
echo "Instalando servicio systemd de usuario..."
sed "s|%h/.cargo/bin/gdsyncd|$BIN_DIR/gdsyncd|g" packaging/systemd/gdsyncd.service > "$SYSTEMD_USER_DIR/gdsyncd.service"

# 8. Refrescar cachés de KDE / XDG si las utilidades existen
if command -v kbuildsycoca6 >/dev/null 2>&1; then
    kbuildsycoca6 --noincremental >/dev/null 2>&1 || true
elif command -v kbuildsycoca5 >/dev/null 2>&1; then
    kbuildsycoca5 --noincremental >/dev/null 2>&1 || true
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
