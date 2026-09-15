# AGENTS.md — Linux Port: LumaForge Ecosystem

> **Fecha:** 2026-09-15
> **Branch:** `feat/linux-port` (los 3 repos)
> **Objetivo:** Portar LumaForge a Linux (Ubuntu/SteamOS)

---

## Resumen del Port

Tres repositorios fueron modificados para soporte Linux:

| Repo | Branch | Commit | Cambios |
|---|---|---|---|
| `lumaforge-cdp-proxy` | `feat/linux-port` | `1b75bfc` | 12 files, +876/-164 |
| `luma-lite` | `feat/linux-port` | `1b0f1cf` | 29 files, +3474/-33 |
| `lumaforge-extensions` | `feat/linux-port` | `1230528` | 1 file, +11/-11 |

---

## Arquitectura General

```
LumaForge en Linux:
┌─────────────────────────────────────────────────────────┐
│  lumaforge-cdp-proxy.so (LD_PRELOAD)                   │
│  ├── hook_linux.rs     → execve hook (inyecta CDP port)│
│  ├── platform.rs       → paths de Steam, config, etc.  │
│  ├── plugin_loader_linux.rs → carga plugins desde disco │
│  ├── ipc.rs            → Unix socket (/tmp/lumaforge)  │
│  ├── discovery.rs      → steam-cdp.json (port discovery)│
│  ├── cdp.rs            → CDP client (100% portable)    │
│  ├── bridge.rs         → HTTP bridge server (portable)  │
│  └── injector.rs       → JS injection via CDP (portable)│
├─────────────────────────────────────────────────────────┤
│  luma-lite (Tauri 2 app)                               │
│  ├── commands/slssteam.rs       → SLS Steam integration│
│  ├── commands/depot_downloader.rs → DepotDownloaderMod  │
│  ├── commands/steam_acf.rs      → ACF generation       │
│  ├── commands/steam_library.rs  → Library management   │
│  ├── commands/manifest_watcher.rs → Manifest backup    │
│  ├── utils/slssteam_config.rs   → YAML config editor  │
│  ├── utils/path_utils.rs        → Steam path detection │
│  └── components/DownloadsView.tsx → Download UI        │
├─────────────────────────────────────────────────────────┤
│  lumaforge-extensions/steam-store-helper               │
│  └── backend.lua → Cross-platform paths (forward slash) │
└─────────────────────────────────────────────────────────┘
```

---

## Repo 1: lumaforge-cdp-proxy

### Ubicación
```
~/Codigo/lumaforge-cdp-proxy/
```

### Qué hace
DLL/.so que se inyecta en Steam para:
1. Hook de `steamwebhelper` → añadir `--remote-debugging-port`
2. Servidor bridge HTTP (TCP 21775) para APIs de LumaLite
3. Exportar tema de Steam para CEF injection
4. Cargar y ejecutar backends Lua de extensiones

### Archivos modificados/creados para Linux

| Archivo | Estado | Qué hace |
|---|---|---|
| `src/platform.rs` | **NUEVO** | Abstracción de paths: `find_steam_install()`, `local_data_dir()`, `config_dir()`, `runtime_dir()`, `plugins_dir()`, `themes_dir()` |
| `src/hook_linux.rs` | **NUEVO** | Loop de inyección CDP: detecta steamwebhelper, publica discovery, conecta CDP, inyecta JS, watchea nuevos targets |
| `src/plugin_loader_linux.rs` | **NUEVO** | Carga plugins desde `~/.local/share/LumaForge/plugins/` (lee manifest.json, extension-config.json, inject.js) |
| `src/lib.rs` | **MODIFICADO** | `#[ctor]` en lugar de `DllMain`, cfg gates para Windows/Linux, `stealth_kill_all_webhelpers()` lee `/proc` |
| `src/ipc.rs` | **MODIFICADO** | Unix domain socket (`/tmp/lumalite_core.sock`) en Linux, named pipes en Windows |
| `src/discovery.rs` | **MODIFICADO** | `atomic_write` con `fs::rename` en Linux (en vez de `MoveFileExW`) |
| `src/theme.rs` | **MODIFICADO** | Usa `platform::themes_dir()` y `platform::runtime_dir()` |
| `src/lua_backend.rs` | **MODIFICADO** | `detect_steam_root()` usa `platform::find_steam_install()`, separadores cross-platform |
| `Cargo.toml` | **MODIFICADO** | Deps condicionales: `libc`/`ctor`/`libloading` en Linux, `windows-sys`/`minhook` en Windows |
| `build.rs` | **MODIFICADO** | Solo compila `cef_hook` en Windows |

### Archivos 100% portables (sin cambios)
- `src/cdp.rs` — Cliente CDP
- `src/bridge.rs` — Servidor HTTP bridge
- `src/injector.rs` — Inyección JS via CDP
- `src/plugin.rs` — Tipos de plugin

### Cómo compilar en Linux

```bash
# Instalar dependencias del sistema (Ubuntu/Debian)
sudo apt-get install -y pkg-config libssl-dev build-essential

# Compilar
cd ~/Codigo/lumaforge-cdp-proxy
cargo build --release

# El .so se genera en:
# target/release/liblumaforge.so
```

### Cómo probar

```bash
# Copiar .so a Steam
cp target/release/liblumaforge.so ~/.steam/steam/

# Lanzar Steam con LD_PRELOAD
LD_PRELOAD=~/.steam/steam/liblumaforge.so steam

# Verificar logs
cat /tmp/steamcdp_proxy.log
```

### Dependencias Linux
- `pkg-config` — para compilar native crates
- `libssl-dev` — para `reqwest` con TLS
- `build-essential` — gcc, etc.

### Issues conocidos
- `hook_linux.rs:95`: `let plugins = Vec::new()` — los plugins no se cargan aún en el loop CDP. Falta integrar `plugin_loader_linux::load_all_plugins()` en el loop.
- `lib.rs:303`: `log_to_temp("[steamcdp] Linux: plugin loading not yet implemented")` — el init constructor no carga plugins aún. Solo logea.
- `build.rs`: No compila `cef_hook` en Linux (correcto, no se necesita).

---

## Repo 2: luma-lite

### Ubicación
```
~/Codigo/luma-lite/
```

### Qué hace
App Tauri 2 (Rust backend + React frontend) que:
- Detecta y gestiona Steam
- Gestiona extensiones/plugins
- Permite descargar juegos con DepotDownloaderMod
- Integra SLS Steam para online multiplayer
- Muestra descargas en una UI dedicada

### Archivos creados para Linux

#### Backend Rust (`src-tauri/src/`)

| Archivo | Líneas | Qué hace |
|---|---|---|
| `commands/slssteam.rs` | ~440 | 14 comandos Tauri para SLS Steam: status, kill, start (LD_AUDIT), API send, patch steam.sh, full setup, config CRUD |
| `commands/depot_downloader.rs` | ~450 | Orquestación de DepotDownloaderMod: resolve depots, start/cancel/pause download, parse Lua manifests |
| `commands/steam_acf.rs` | ~170 | Generación de `appmanifest_{appid}.acf` con soporte Proton (platform_override) |
| `commands/steam_library.rs` | ~300 | Detección de librerías Steam, instalación de juegos, movimiento de manifests, actualización de `libraryfolders.vdf` |
| `commands/manifest_watcher.rs` | ~170 | Watcher de `notify` que detecta borrado de manifests y los restaura desde backup |
| `utils/slssteam_config.rs` | ~310 | Manipulación YAML de `~/.config/SLSsteam/config.yaml`: AdditionalApps, AppTokens, FakeAppIds, normalización |
| `utils/path_utils.rs` | ~150 | Detección de paths de Steam: `detect_steam_paths()` retorna `SteamPaths` struct |
| `utils/lua_parser.rs` | ~175 | Parser de archivos Lua de DepotDownloaderMod |
| `models/depot.rs` | ~60 | Modelos de datos para depots |
| `models/steam_paths.rs` | ~15 | Modelo `SteamPaths` |

#### Frontend React (`src/`)

| Archivo | Qué hace |
|---|---|
| `components/DownloadsView.tsx` | Vista principal de descargas: tabla con activas/historial, listeners de eventos Tauri |
| `components/DownloadStatusBadge.tsx` | Badge con colores por status (pending/downloading/complete/error) |
| `components/DownloadProgressBar.tsx` | Barra de progreso animada |
| `components/NewDownloadModal.tsx` | Modal para nueva descarga (app ID, directorio) |
| `types/downloads.ts` | Tipos TypeScript: `DownloadJob`, `DownloadStatus`, etc. |

#### Archivos modificados

| Archivo | Cambio |
|---|---|
| `src-tauri/src/commands/thirdparty.rs` | Extendido `ToolDef` con `linux_github_owner`, `linux_github_repo`, `linux_preferred_asset`, `variants`. DepotDownloaderMod: Windows=`mendy-tools`, Linux=`eisora08` |
| `src-tauri/src/commands/game_fix.rs` | Adaptado para Linux (stubs para SmokeAPI/Steamless que no existen en Linux) |
| `src-tauri/src/commands/mod.rs` | +5 nuevos módulos: `slssteam`, `steam_acf`, `steam_library`, `manifest_watcher`, `depot_downloader` |
| `src-tauri/src/lib.rs` | +25 comandos registrados + ManifestWatcherState |
| `src-tauri/src/models/mod.rs` | +`depot`, `steam_paths` |
| `src-tauri/src/utils/mod.rs` | +`lua_parser`, `path_utils`, `slssteam_config` |
| `src-tauri/Cargo.toml` | +`notify = "6"`, `regex`, `uuid` |
| `src/components/Sidebar.tsx` | Nav item "Downloads" con icono |
| `src/App.tsx` | Routing para DownloadsView |
| `src/App.css` | ~400 líneas de estilos para Downloads |
| `src/types.ts` | `'downloads'` añadido a `ViewId` |

### Cómo compilar en Linux

```bash
# Instalar dependencias del sistema (Ubuntu/Debian)
sudo apt-get install -y \
  pkg-config libssl-dev libgtk-3-dev libwebkit2gtk-4.1-dev \
  libayatana-appindicator3-dev librsvg2-dev patchelf \
  libjavascriptcoregtk-4.1-dev libsoup-3.0-dev

# Compilar
cd ~/Codigo/luma-lite/src-tauri
cargo check  # Verificar
cargo build --release  # Compilar

# El binario se genera en:
# target/release/luma-lite
```

### Cómo probar

```bash
# Ejecutar directamente
./target/release/luma-lite

# O con Tauri dev mode
cd ~/Codigo/luma-lite
npm install
npm run tauri dev
```

### Issues conocidos
- `thirdparty.rs`: `name` field en `ToolVariant` nunca se lee (warning)
- `thirdparty.rs`: `linux_github_owner`, `linux_github_repo`, `linux_preferred_asset` nunca se leen en Windows (warnings esperados, solo se usan en Linux)
- `depot_downloader.rs`: `SILENCE_TIMEOUT_SECS` y `PROGRESS_THROTTLE_MS` constantes no usadas
- `game_fix.rs`: `tag_name` field nunca se lee
- `library.rs`: Varias funciones y fields no leídos (pre-existentes, no del port)
- `window_fx.rs`: En Linux es stub (no Mica/Acrylic) — esto es correcto
- `system_toggle.rs`: En Linux debería usar `.so` + LD_AUDIT en vez de DLL injection — verificar que esté implementado

---

## Repo 3: lumaforge-extensions

### Ubicación
```
~/Codigo/lumaforge-extensions/
```

### Qué hace
Extensiones Lua que se ejecutan dentro del CDP Proxy. La extensión `steam-store-helper` provee:
- Detección de juegos en Steam
- Descarga de manifiestos
- Gestión de library folders

### Archivo modificado

| Archivo | Cambio |
|---|---|
| `extensions/steam-store-helper/backend.lua` | 10 paths cambiados de `\` a `/` (backslash → forward slash) |

### Paths corregidos
```lua
-- ANTES (Windows):
lad .. "\\LumaForge\\config.json"
steam .. "\\config\\lua\\" .. app_id .. ".lua"

-- DESPUÉS (cross-platform):
lad .. "/LumaForge/config.json"
steam .. "/config/lua/" .. app_id .. ".lua"
```

### Sin cambios necesarios
- `manifest.json` — ya es cross-platform
- `extension.lua` — ya es cross-platform
- `inject.js` — JS puro, cross-platform

---

## Flujo de Ejecución en Linux

```
1. Usuario ejecuta: LD_PRELOAD=liblumaforge.so steam

2. liblumaforge.so se carga (#[ctor] init):
   ├── Lee /proc/self/cmdline para identificar proceso
   ├── Mata steamwebhelper existentes (pkill)
   ├── Exporta tema de Steam
   ├── Inicia IPC server (Unix socket)
   ├── Inicia bridge server (TCP 21775)
   └── Inicia CDP injection loop

3. CDP injection loop:
   ├── Publica discovery (steam-cdp.json o TCP)
   ├── Espera a que CDP esté disponible
   ├── Conecta a CDP
   ├── Inyecta JS en targets existentes
   └── Watchea nuevos targets y los inyecta

4. LumaLite detecta CDP:
   ├── Lee steam-cdp.json o conecta TCP 21775
   ├── Muestra UI con plugins/extensiones
   └── Permite gestionar SLS Steam + DepotDownloader
```

---

## Comandos Tauri Registrados (25 nuevos)

### SLS Steam (14 comandos)
```rust
slssteam_status                    // Estado de SLS Steam
slssteam_kill_steam                // Matar proceso Steam
slssteam_start_steam               // Lanzar Steam con LD_AUDIT
slssteam_api_send                  // Enviar comando por pipe
slssteam_patch_steam_sh            // Parchear steam.sh
slssteam_full_setup                // Setup completo
slssteam_config_add_additional_app // Añadir juego a AdditionalApps
slssteam_config_remove_additional_app
slssteam_config_add_app_token      // Añadir AppToken
slssteam_config_add_fake_app_id    // Añadir FakeAppId
slssteam_config_remove_fake_app_id
slssteam_config_is_in_additional_apps
slssteam_config_get_additional_apps
slssteam_config_fix_indentation
```

### Steam Infrastructure (8 comandos)
```rust
steam_acf_create                    // Crear appmanifest ACF
steam_library_install_game          // Instalar juego en librería
steam_library_move_manifests        // Mover manifests a depotcache
steam_library_update_vdf            // Actualizar libraryfolders.vdf
steam_library_detect                // Detectar librerías Steam
steam_library_ensure_structure      // Crear dirs steamapps/
start_manifest_watcher              // Iniciar watcher
stop_manifest_watcher               // Detener watcher
get_manifest_watcher_status         // Estado del watcher
```

### Depot Downloader (7 comandos)
```rust
depot_downloader_resolve_depots     // Resolver depots de un juego
depot_downloader_start              // Iniciar descarga
depot_downloader_cancel             // Cancelar descarga
depot_downloader_pause              // Pausar descarga
depot_downloader_status             // Estado de descarga
depot_downloader_default_output_dir // Directorio por defecto
depot_downloader_parse_lua_manifests // Parsear manifiestos Lua
```

---

## Dependencias del Sistema (Ubuntu/Debian)

```bash
# CDP Proxy
sudo apt-get install -y pkg-config libssl-dev build-essential

# LumaLite (Tauri 2)
sudo apt-get install -y \
  pkg-config libssl-dev \
  libgtk-3-dev libwebkit2gtk-4.1-dev \
  libayatana-appindicator3-dev librsvg2-dev \
  patchelf libjavascriptcoregtk-4.1-dev libsoup-3.0-dev

# General
sudo apt-get install -y curl git gcc g++ make
```

---

## Estructura de Directorios en Linux

```
~/.local/share/LumaForge/
├── runtime/          # steam-cdp.json, theme files
├── plugins/          # Extensiones instaladas
│   └── steam-store-helper/
│       ├── manifest.json
│       ├── extension.lua
│       ├── backend.lua
│       └── inject.js
└── themes/           # Temas exportados

~/.config/LumaForge/
└── config.json       # Configuración principal

~/.config/SLSsteam/
└── config.yaml       # Configuración de SLS Steam

~/.steam/steam/       # Steam installation
├── steam.sh          # Parcheado por slssteam_patch_steam_sh
├── steam.cfg         # Creado para bloquear actualizaciones
├── steamapps/
│   ├── common/       # Juegos instalados
│   └── depotcache/   # Manifests
└── config/
    └── lua/          # Lua scripts de extensiones

/tmp/
├── steamcdp_proxy.log    # Logs del CDP proxy
├── lumalite_core.sock    # IPC Unix socket
└── SLSsteam.API          # Pipe de API de SLS Steam
```

---

## Pasos para Probar en Linux

### 1. Clonar y compilar CDP Proxy
```bash
git clone -b feat/linux-port https://github.com/eisora08/lumaforge-cdp-proxy.git
cd lumaforge-cdp-proxy
sudo apt-get install -y pkg-config libssl-dev build-essential
cargo build --release
# Verificar: target/release/liblumaforge.so existe
```

### 2. Clonar y compilar LumaLite
```bash
git clone -b feat/linux-port https://github.com/eisora08/luma-lite.git
cd luma-lite/src-tauri
sudo apt-get install -y libgtk-3-dev libwebkit2gtk-4.1-dev libayatana-appindicator3-dev
cargo check
# Si hay errores, verificar dependencias del sistema
```

### 3. Clonar extensiones
```bash
git clone -b feat/linux-port https://github.com/eisora08/lumaforge-extensions.git
# Copiar steam-store-helper a ~/.local/share/LumaForge/plugins/
mkdir -p ~/.local/share/LumaForge/plugins
cp -r lumaforge-extensions/extensions/steam-store-helper ~/.local/share/LumaForge/plugins/
```

### 4. Probar CDP Proxy
```bash
# Copiar .so a Steam
cp lumaforge-cdp-proxy/target/release/liblumaforge.so ~/.steam/steam/

# Lanzar Steam con proxy
LD_PRELOAD=~/.steam/steam/liblumaforge.so steam &

# Verificar logs
sleep 5
cat /tmp/steamcdp_proxy.log

# Verificar discovery
cat ~/.local/share/LumaForge/runtime/steam-cdp.json
```

### 5. Probar LumaLite
```bash
cd luma-lite/src-tauri
cargo run
# O desde la raíz de luma-lite:
# npm run tauri dev
```

---

## Solución de Problemas

### "pkg-config not found" o "libssl-dev not found"
```bash
sudo apt-get install -y pkg-config libssl-dev
```

### "webkit2gtk-4.1 not found"
```bash
sudo apt-get install -y libwebkit2gtk-4.1-dev libsoup-3.0-dev libjavascriptcoregtk-4.1-dev
```

### "libayatana-appindicator not found"
```bash
sudo apt-get install -y libayatana-appindicator3-dev
```

### CDP proxy no se conecta
1. Verificar que Steam esté corriendo: `pgrep steam`
2. Verificar que `steam-cdp.json` exista: `cat ~/.local/share/LumaForge/runtime/steam-cdp.json`
3. Verificar logs: `cat /tmp/steamcdp_proxy.log`
4. Verificar que el .so se cargó: `LD_DEBUG=libs LD_PRELOAD=liblumaforge.so steam 2>&1 | head -50`

### LumaLite no detecta Steam
1. Verificar que Steam esté instalado: `ls ~/.steam/steam/`
2. Verificar Flatpak: `ls ~/.var/app/com.valvesoftware.Steam/data/Steam/`
3. Verificar config: `cat ~/.config/LumaForge/config.json`

### SLS Steam no funciona
1. Verificar que `steam.sh` fue parcheado: `head -15 ~/.steam/steam/steam.sh | grep LD_AUDIT`
2. Verificar config: `cat ~/.config/SLSsteam/config.yaml`
3. Verificar que SLSsteam.so esté instalado: `find ~ -name "SLSsteam.so" 2>/dev/null`

---

## TODO / Pendiente

- [ ] `hook_linux.rs:95` — Integrar `plugin_loader_linux::load_all_plugins()` en el loop CDP
- [ ] `lib.rs:303` — Implementar carga de plugins en el init constructor de Linux
- [ ] `system_toggle.rs` — Implementar toggle para Linux (LD_AUDIT en vez de DLL injection)
- [ ] `window_fx.rs` — Implementar efectos de ventana en Linux (o confirmar que stub es suficiente)
- [ ] `steam_library.rs:300` — `get_disk_space()` retornar 0, implementar con `libc::statvfs`
- [ ] Testing completo: CDP injection, theme export, plugin loading, SLS Steam, DepotDownloader
- [ ] Packaging: `.deb` y/o AppImage para LumaLite
- [ ] Script de instalación para CDP proxy

---

## Referencias

| Recurso | Ubicación |
|---|---|
| LumaForge Linux port (referencia) | `LumaForge` branch `feat/linux-port` |
| CDP Proxy GitHub | https://github.com/eisora08/lumaforge-cdp-proxy |
| LumaLite GitHub | https://github.com/eisora08/luma-lite |
| Extensions GitHub | https://github.com/eisora08/lumaforge-extensions |
| SLS Steam (Linux) | https://github.com/eisora08/SLSsteam |
| DepotDownloaderMod (Linux) | https://github.com/eisora08/DepotDownloaderMod |
