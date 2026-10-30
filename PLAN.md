# Plan de Arquitectura y Desarrollo: gdsync

Cliente híbrido de Google Drive para Linux (optimizado para KDE/Dolphin), con sincronización selectiva, soporte bajo demanda, interfaz gráfica (GUI) y CLI, desarrollado en **Rust** y publicado bajo código abierto en GitHub.

---

## 1. Visión del Proyecto y Filosofía

- **Lenguaje:** **Rust** (alto rendimiento, seguridad de memoria estricta y mínimo consumo de RAM para un proceso 24/7).
- **Modo híbrido:** El usuario ve su estructura de Google Drive en una carpeta local de su elección (por defecto `~/GoogleDrive`, totalmente configurable desde la GUI o CLI a cualquier ruta o disco secundario) sin necesidad de descargar todos los archivos previamente.
- **Ubicación personalizable:** Posibilidad de elegir libremente la carpeta de montaje o sincronización local (en disco principal, secundario o partición dedicada).
- **Bajo demanda (On-demand):** Un archivo se descarga a la caché local cuando una aplicación o el usuario lo abre.
- **Sincronización selectiva:** Permite sincronizar todo el Drive (`root`) o elegir únicamente subcarpetas específicas (ej. solo `/Trabajo` y `/Proyectos`).
- **Interfaces de usuario:**
  - **CLI (`gdsync`):** Control rápido por terminal y scripts.
  - **GUI (`gdsync-gui`):** Ventana de configuración visual, selector de carpetas con checkboxes, estado de almacenamiento y monitor de transferencias.
  - **Bandeja del sistema (Tray icon):** Menú rápido junto al reloj con estado, pausa y accesos directos.
- **Disponibilidad sin conexión (Pinning):** Archivos o carpetas marcadas como "disponibles sin conexión" permanecen siempre en caché y no se eliminan por limpieza automática.
- **Google Docs / Sheets / Slides:** Se representan como archivos `.desktop` (`.gdoc.desktop`, `.gsheet.desktop`, etc.) que abren el documento en el navegador web.
- **Distribución:** Proyecto público en GitHub bajo licencia **GPL-3.0**. El usuario final puede ingresar su propio OAuth Client ID/Secret o usar el modo de pruebas (Testing Mode).

---

## 2. Arquitectura General

```text
┌────────────────────────────────────────────────────────────────────────┐
│                         Capa de Presentación                           │
│                                                                        │
│   ┌─────────────────────┐   ┌─────────────────┐   ┌────────────────┐   │
│   │     Dolphin         │   │   gdsync-gui    │   │     gdsync     │   │
│   │ - Menú contextual   │   │ - Selector arbóreo│ │    (CLI)       │   │
│   │ - Emblemas de estado│   │ - Ajustes/Storage │ │                │   │
│   │ - KOverlayPlugin    │   │ - Estado en vivo│ │                │   │
│   └──────────┬──────────┘   └────────┬────────┘   └───────┬────────┘   │
│              │                       │                    │            │
│              └───────────────────────┼────────────────────┘            │
│                                      │ D-Bus (org.gdsync.Daemon)       │
└──────────────────────────────────────┼─────────────────────────────────┘
                                       │
┌──────────────────────────────────────▼─────────────────────────────────┐
│                      Daemon en segundo plano (gdsyncd)                 │
│                                                                        │
│  ┌───────────────────────┐   ┌──────────────────────────────────────┐  │
│  │ Capa FUSE (Virtual FS)│   │  Motor de Sync                       │  │
│  │ - Intercepta read/open│   │  - Changes API (polling periódico)   │  │
│  │ - Bloquea indexadores │   │  - Cola de subidas/descargas         │  │
│  └───────────┬───────────┘   └──────────────────┬───────────────────┘  │
│              │                                  │                      │
│  ┌───────────▼──────────────────────────────────▼───────────────────┐  │
│  │ SQLite (state.db)                                                │  │
│  │ - Árbol de metadatos (inodos estables)                           │  │
│  │ - Filtros de carpetas seleccionadas                              │  │
│  │ - Estado de caché y archivos fijados (pinned)                    │  │
│  └──────────────────────────────────────────────────────────────────┘  │
│  ┌──────────────────────────────────────────────────────────────────┐  │
│  │ Caché Local de Contenido (~/.cache/gdsync/blobs)                 │  │
│  │ - Archivos descargados bajo demanda                              │  │
│  │ - Política LRU con límite de gigabytes configurable              │  │
│  └──────────────────────────────────────────────────────────────────┘  │
└──────────────────────────────────────▲─────────────────────────────────┘
                                       │ HTTPS (Drive API v3)
┌──────────────────────────────────────▼─────────────────────────────────┐
│                        Google Drive API v3                             │
└────────────────────────────────────────────────────────┘
```

---

## 3. Componentes del Workspace (Rust)

1. **`crates/core` (Librería compartida):**
   - `auth.rs`: OAuth 2.0 PKCE, tokens y refresh.
   - `drive.rs`: Cliente REST de la API de Google Drive v3.
   - `db.rs`: Base de datos SQLite para metadatos locales y configuración de carpetas.
   - `cache.rs`: Gestión de archivos locales bajo demanda con política LRU.
   - `names.rs`: Normalización de nombres de archivo y enlaces `.desktop` para Google Docs.
   - `sync.rs`: Sincronización inicial y consulta de cambios incrementales (Changes API).

2. **`crates/daemon` (`gdsyncd`):**
   - Montaje del sistema de archivos FUSE (`fuser`).
   - Servidor D-Bus (`zbus`) exponiendo métodos: `GetStatus`, `AddFolder`, `RemoveFolder`, `PinPath`, `UnpinPath`, `Pause`, `Resume`.
   - Control anti-indexadores (Baloo / thumbnails).

3. **`crates/cli` (`gdsync`):**
   - Comandos de terminal: `login`, `logout`, `status`, `ls`, `folder add/remove/list`, `pin`, `unpin`.

4. **`crates/gui` (`gdsync-gui`):**
   - Interfaz de usuario gráfica moderna y liviana (utilizando **Slint** o **Tauri/HTML** conectado a D-Bus).
   - **Pestaña Estado:** Espacio usado en Drive vs. espacio en disco local, estado de conexión, cola de transferencias activas.
   - **Pestaña Sincronización Selectiva:** Vista en árbol con casillas de verificación para tildar las carpetas que se desean ver en la PC.
   - **Pestaña Ajustes:** Límite de tamaño de caché (GB), ruta de montaje (`~/GoogleDrive`), configuración de credenciales OAuth.
   - **Autenticación asistida:** Botón para iniciar sesión y asistente de configuración inicial.

5. **`crates/tray` (Bandeja del sistema):**
   - Ícono StatusNotifierItem (KDE/GNOME) que muestra el estado de sincronización y menú contextual con opciones rápidas (Pausar, Abrir carpeta, Abrir GUI, Salir).

6. **Integración con Dolphin:**
   - Menú contextual de KDE (`.desktop` ServiceMenu) para fijar/desfijar archivos ("Disponible sin conexión" / "Liberar espacio").
   - Plugin de emblemas (`KOverlayIconPlugin` en C++/Qt) para ver íconos de estado sobre los archivos en Dolphin.

---

## 4. Fases de Implementación

- **Fase 1: Motor Base y Autenticación**
  - Configuración OAuth con pantalla gráfica/CLI y almacenamiento seguro de token.
  - Listado remoto y configuración de carpetas selectivas en base de datos.
- **Fase 2: Metadatos y Montaje Virtual (FUSE Solo Lectura)**
  - Construcción del árbol local en SQLite solo con las carpetas elegidas.
  - Montaje en `~/GoogleDrive` con descarga bajo demanda al abrir archivos.
- **Fase 3: Cambios Remotos y Sincronización Continua**
  - Monitoreo periódico con la Changes API.
  - Actualización reactiva del árbol de archivos.
- **Fase 4: Escritura Local y Cola de Subidas**
  - Guardado de nuevos archivos y modificaciones locales.
  - Subidas reanudables y manejo seguro de papelera.
- **Fase 5: Interfaz Gráfica (GUI) y Bandeja de Sistema**
  - Desarrollo de `gdsync-gui` con selector de carpetas en árbol y panel de estado.
  - Integración del tray icon con notificaciones de escritorio.
- **Fase 6: Integración Profunda con Dolphin y Empaquetado**
  - ServiceMenu y plugin de emblemas de estado.
  - Servicio `systemd --user` y preparación para distribución en GitHub (`.deb`, AUR, release binaries).
