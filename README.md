# gdsync

Google Drive en tu gestor de archivos (Linux, KDE primero), en **modo híbrido**:
ves todo tu Drive como una carpeta (`~/GoogleDrive`) y cada archivo se descarga
recién cuando lo abrís.

> Estado: **fases 0–2** del plan. El montaje es de **solo lectura**: podés navegar, abrir
> archivos (se descargan y quedan en caché) y los cambios hechos en la web aparecen
> solos. Escritura, "Disponible sin conexión" e íconos en Dolphin vienen en las fases siguientes.

## Requisitos

```bash
sudo apt install build-essential fuse3      # compilador C (sqlite, TLS) y fusermount3
curl https://sh.rustup.rs -sSf | sh          # Rust
```

## 1. Crear tu credencial de Google (una sola vez)

1. Entrá a <https://console.cloud.google.com/> y creá un proyecto (por ejemplo `gdsync`).
2. **APIs y servicios → Biblioteca**: habilitá **Google Drive API**.
3. **APIs y servicios → Pantalla de consentimiento de OAuth**: tipo **Externo**, completá
   nombre y correo, y en **Usuarios de prueba** agregá tu cuenta de Google.
4. **Credenciales → Crear credenciales → ID de cliente de OAuth → App de escritorio**.
   Copiá el *Client ID* y el *Client secret*.

> [!TIP]
> Mientras desarrollamos, usá una **cuenta de Google de prueba**.

## 2. Compilar e instalar

```bash
cargo install --path crates/cli      # gdsync
cargo install --path crates/daemon   # gdsyncd
```

## 3. Iniciar sesión y montar

```bash
gdsync login --client-id TU_CLIENT_ID --client-secret TU_CLIENT_SECRET
gdsync ls          # prueba rápida: lista la raíz de tu Drive
gdsyncd            # monta en ~/GoogleDrive (Ctrl-C para desmontar)
```

Para que se monte solo al iniciar sesión:

```bash
mkdir -p ~/.config/systemd/user
cp packaging/systemd/gdsyncd.service ~/.config/systemd/user/
systemctl --user enable --now gdsyncd
journalctl --user -u gdsyncd -f      # ver el log
```

Excluí la carpeta del indexador de KDE para que no intente leer todo:

```bash
balooctl6 config add excludeFolders ~/GoogleDrive
```

## Comandos

| Comando | Qué hace |
|---|---|
| `gdsync login` | Inicia sesión (la primera vez con `--client-id`/`--client-secret`) |
| `gdsync ls` | Lista la raíz de Drive directamente desde la API |
| `gdsync status` | Punto de montaje, sesión, cantidad de elementos y uso de caché |
| `gdsync logout` | Cierra sesión y revoca el acceso |
| `gdsync resync` | Borra el estado local (con el daemon detenido) |

## Dónde guarda cosas

| Qué | Dónde |
|---|---|
| Configuración y token | `~/.config/gdsync/` (archivos `0600`) |
| Árbol de Drive (SQLite) | `~/.local/share/gdsync/state.db` |
| Contenido descargado | `~/.cache/gdsync/blobs/` (límite: `cache_max_gb` en `config.json`, por defecto 10) |

## Cómo se ven las cosas

- **Google Docs/Sheets/Slides**: aparecen como `Nombre.gdoc.desktop` (o `.gsheet`, `.gslides`…). Al abrirlos se abren en el navegador.
- **Nombres repetidos** en una carpeta: `foto.jpg`, `foto (2).jpg`…
- **`/` en un nombre**: se muestra como `∕`.
- Los **atajos** de Drive todavía no se muestran.

## Desarrollo

```bash
cargo test
RUST_LOG=debug cargo run -p gdsyncd
```
