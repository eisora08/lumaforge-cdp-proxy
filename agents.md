# AGENTS.md — Linux Port: LumaForge Ecosystem

> **Fecha:** 2026-09-18 (última actualización)
> **Branch:** `feat/linux-port` (los 3 repos)
> **Objetivo:** Portar LumaForge a Linux (Nobara 44 / Fedora-based)

---

## Resumen del Port

Tres repositorios fueron modificados para soporte Linux:

| Repo | Branch | Último Commit | Cambios |
|---|---|---|---|
| `lumaforge-cdp-proxy` | `feat/linux-port` | `05856e6` | 12+ files, CDP injection completa |
| `luma-lite` | `feat/linux-port` | `1b0f1cf` | 29 files, +3474/-33 |
| `lumaforge-extensions` | `feat/linux-port` | `1230528` | 1 file, +11/-11 |

### Commits del CDP Proxy (orden cronológico)
```
05856e6 feat: URL change detection + exponential CDP backoff + optimized bridge drain
bf7195a feat: Millennium-style libXtst.so.6 proxy + pvs_shim + CDP pipe client
a016f74 fix: reduce CDP load to match vanilla Steam lifetime + filter Shutdown targets
3b0cb6f feat: bridge proxy for mixed-content fix + hook subcrate + diagnostics
ecf9064 feat: working Linux CDP injection with re-injection, diagnostic eval, panic hook
de01033 docs: add agents.md for Linux port context
1b75bfc feat: Linux port — cross-platform CDP proxy
```

---

## Arquitectura General

```
LumaForge en Linux:
┌─────────────────────────────────────────────────────────┐
│  libXtst.so.6 proxy (hook/src/lib.rs)                  │
│  ├── Steam carga automáticamente (reemplaza libXtst)   │
│  ├── Forward XTest functions via dlsym                  │
│  └── dlopen liblumaforge.so en ctor                    │
├─────────────────────────────────────────────────────────┤
│  liblumaforge.so (#[ctor] init)                        │
│  ├── hook_linux.rs     → CDP injection loop (1s cycle) │
│  │   ├── Exponential backoff: 100ms → 500ms            │
│  │   ├── URL change detection (known_urls HashMap)     │
│  │   ├── Bridge drain every 1s                         │
│  │   ├── make_bridge_request: tries 21777 then 21775   │
│  │   └── Recheck all targets every 30s (store first)  │
│  ├── injector.rs       → JS injection via CDP          │
│  │   ├── BRIDGE_PROXY_JS (mixed-content fix)           │
│  │   ├── addScriptToEvaluateOnNewDocument              │
│  │   └── Runtime.evaluate for immediate inject         │
│  ├── platform.rs       → paths de Steam, config, etc.  │
│  ├── plugin_loader_linux.rs → carga plugins desde disco │
│  ├── ipc.rs            → Unix socket (/tmp/lumalite_core.sock)│
│  ├── discovery.rs      → steam-cdp.json (port discovery)│
│  ├── cdp.rs            → CDP client (TCP, reqwest)     │
│  ├── bridge.rs         → HTTP bridge server (port 21775)│
│  ├── lua_backend.rs    → Lua 5.4 sandboxed engine      │
│  └── theme.rs          → Theme export for CEF          │
├─────────────────────────────────────────────────────────┤
│  luma-lite (Tauri 2 app) — PUERTO 21775                │
│  ├── commands/steam_bridge.rs → HTTP bridge (real data) │
│  ├── commands/slssteam.rs     → SLS Steam integration  │
│  └── ... (ver sección luma-lite)                        │
├─────────────────────────────────────────────────────────┤
│  lumaforge-extensions/steam-store-helper               │
│  └── inject.js → Bridge fetch via proxy (HTTPS→HTTP)   │
└─────────────────────────────────────────────────────────┘
```

### Flujo de datos (bridge proxy)

```
1. Steam CEF loads store.steampowered.com (HTTPS)
2. inject.js intercepts fetch() to 127.0.0.1:21775
3. Request queued in __lumaBridgeQueue (can't fetch HTTP from HTTPS)
4. CDP proxy drains queue every 1s via Runtime.evaluate
5. Rust makes HTTP request — tries 21777 (luma-lite) first, then 21775 (stub)
6. Response injected back via Runtime.evaluate
7. JS Promise resolves with the response
```

### Desktop launch (sin LD_PRELOAD)

```
~/.local/share/Steam/ubuntu12_32/libXtst.so.6  ← proxy (32-bit)
~/.local/share/Steam/ubuntu12_32/liblumaforge.so ← main library (32-bit)

Steam carga libXtst.so.6 automáticamente (necesita XTest).
El proxy hace dlopen de liblumaforge.so en el ctor.
No se necesita LD_PRELOAD.
```

---

## Repo 1: lumaforge-cdp-proxy

### Ubicación
```
~/Codigo/lumaforge-cdp-proxy/
```

### Qué hace
DLL/.so que se inyecta en Steam para:
1. **libXtst.so.6 proxy** — Steam carga automáticamente; forward XTest + dlopen liblumaforge.so
2. **CDP injection loop** — conecta a Steam CEF debug port, inyecta JS
3. **Bridge proxy** — resuelve mixed-content (HTTPS→HTTP) via queue drain
4. **Servidor bridge HTTP** (TCP 21775) — stubs cuando luma-lite no está corriendo
5. **IPC server** — Unix socket para control remoto (themes, plugins)
6. **Exportar tema de Steam** para CEF injection
7. **Cargar y ejecutar backends Lua** de extensiones

### Descubrimientos Importantes

#### 1. Steam binary path
```
~/.local/share/Steam/ubuntu12_32/steam  (32-bit)
steamwebhelper es 64-bit, lanzado via srt-bwrap → pv-adverb → steamwebhelper_sniper_wrap.sh
```

#### 2. pv-adverb no honors PRESSURE_VESSEL_PREFIX
pv-adverb tiene `--prefix=/usr/lib/pressure-vessel/from-host` hardcoded. El approach de pasar FDs via FIFOs no funciona porque el container no tiene acceso a los FDs del host.

#### 3. CreateSimpleProcess inline hook NO funciona
El hook intentaba saltar 5 bytes (E9 jump) sobre `push rbp; push rbx; sub rsp,0x18; call CreateSimpleProcess`. Pero el call tiene 5 bytes (E8 XX XX XX XX), y saltar a fn+5 cae mid-instruction (0x4C = `dec rsp`). El 4to push sobreescribe el callee del call. **Eliminado** — no se necesita con script patching + proxy.

#### 4. Bridge proxy resuelve mixed-content
HTTPS store pages no pueden hacer fetch() a HTTP. Solución:
- `BRIDGE_PROXY_JS` intercepta fetch, encola en `__lumaBridgeQueue`
- CDP proxy drainea la cola cada 1s via `Runtime.evaluate`
- Rust hace el request HTTP a 21777 (luma-lite) primero, 21775 (stub) como fallback
- Resultado se inyecta de vuelta via `Runtime.evaluate`

#### 5. addScriptToEvaluateOnNewDocument NO re-fire en SPA
En Steam CEF, `Page.addScriptToEvaluateOnNewDocument` no se re-ejecuta en navigaciones SPA. Solución: recheck cada 30s re-evalúa `hasLuma` y re-inyecta si es false.

#### 6. URL change detection
`known_urls` HashMap trackea la URL de cada target. Cuando cambia (store page navigation), se re-inyecta inmediatamente sin esperar al recheck de 30s.

#### 7. --disable-web-security causa crash
El flag `--remote-debugging-port` funciona. `--disable-web-security` causa crash del webhelper. No usar.

#### 8. Steam lifetime
Steam en este sistema vive ~60-90s. Matches vanilla lifetime. No es causado por nuestro código.

### Archivos modificados/creados para Linux

| Archivo | Estado | Qué hace |
|---|---|---|
| `hook/src/lib.rs` | **MODIFICADO** | libXtst.so.6 proxy: XTest pass-through + dlopen liblumaforge.so. INIT_DONE guard. Macro-based XTest functions. |
| `src/hook_linux.rs` | **NUEVO** | CDP injection loop: exponential backoff (100ms→500ms), URL change detection, bridge drain 1s, make_bridge_request priority [21777, 21775], recheck 30s all targets |
| `src/injector.rs` | **MODIFICADO** | BRIDGE_PROXY_JS (mixed-content fix), diagnostic eval, theme patches |
| `src/platform.rs` | **NUEVO** | Abstracción de paths: `find_steam_install()`, `local_data_dir()`, `config_dir()`, `runtime_dir()` |
| `src/plugin_loader_linux.rs` | **NUEVO** | Carga plugins desde `~/.local/share/LumaForge/plugins/` |
| `src/lib.rs` | **MODIFICADO** | `#[ctor]` init, panic hook, 6 threads (kill, theme+ipc, plugins, bridge, patch script, CDP loop) |
| `src/ipc.rs` | **MODIFICADO** | Unix domain socket (`/tmp/lumalite_core.sock`) en Linux |
| `src/discovery.rs` | **MODIFICADO** | `OnceLock` cache, `atomic_write` con `fs::rename` |
| `src/cdp_pipe.rs` | **NUEVO** | Pipe CDP client (null-byte delimited JSON over FIFOs). **No integrado aún.** |
| `pvs_shim/pvs_shim.c` | **NUEVO** | 64-bit C binary for pressure-vessel shim. **No funcional** (pv-adverb hardcoded prefix). |
| `Cargo.toml` | **MODIFICADO** | Deps condicionales: `libc`/`ctor`/`libloading` en Linux, `windows-sys`/`minhook` en Windows |

### Archivos 100% portables (sin cambios)
- `src/cdp.rs` — Cliente CDP (TCP, reqwest)
- `src/bridge.rs` — Servidor HTTP bridge (stubs)
- `src/plugin.rs` — Tipos de plugin
- `src/lua_backend.rs` — Lua sandbox
- `src/theme.rs` — Theme management

### Cómo compilar en Linux (32-bit)

```bash
# Instalar dependencias del sistema
sudo dnf install -y openssl-devel.i686  # Fedora/Nobara
# or: sudo apt-get install -y pkg-config libssl-dev build-essential gcc-multilib

# Compilar proxy principal (32-bit) — el build.rs compila el hook subcrate automáticamente
cd ~/Codigo/lumaforge-cdp-proxy
CC_i686_unknown_linux_gnu="gcc -m32" OPENSSL_DIR=/usr OPENSSL_LIB_DIR=/usr/lib OPENSSL_INCLUDE_DIR=/usr/include \
  cargo build --release --target i686-unknown-linux-gnu

# El .so se genera en:
# target/i686-unknown-linux-gnu/release/liblumaforge.so
```

### Cómo deployear

```bash
# Copiar solo liblumaforge.so (libXtst.so.6 no se cambia)
cp target/i686-unknown-linux-gnu/release/liblumaforge.so \
   ~/.local/share/Steam/ubuntu12_32/

# Steam carga libXtst.so.6 automáticamente (no necesita LD_PRELOAD)
# Solo lanzar Steam normalmente:
steam
```

### Verificar logs

```bash
# Ver CDP proxy log
tail -f /tmp/steamcdp_proxy.log

# Buscar errores
grep -i error /tmp/steamcdp_proxy.log

# Verificar connection
grep "Connected to CDP" /tmp/steamcdp_proxy.log

# Verificar inyección
grep "Injected" /tmp/steamcdp_proxy.log

# Verificar bridge
grep "BRIDGE" /tmp/steamcdp_proxy.log
```

### Issues conocidos / Resueltos

| Issue | Estado | Notas |
|---|---|---|
| `hook_linux.rs:95` plugins no se cargan | **RESUELTO** | `load_all_plugins()` integrado en init + recheck |
| `lib.rs:303` carga de plugins no implementada | **RESUELTO** | Thread 3 en init carga plugins + lua backends |
| CreateSimpleProcess inline hook SEGV | **RESUELTO** | Eliminado — E9 jump sobreescribe mid-instruction |
| pv-adverb hardcoded prefix | **PENDIENTE** | No se puede pasar FDs al container |
| CDP pipe client no integrado | **PENDIENTE** | `cdp_pipe.rs` existe pero no se usa |
| `window_fx.rs` stub en Linux | **OK** | No hay equivalente Linux para Mica/Acrylic |
| `system_toggle.rs` DLL injection | **PENDIENTE** | Necesita LD_AUDIT equivalent en luma-lite |

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
1. Usuario ejecuta: steam (sin LD_PRELOAD)

2. Steam carga libXtst.so.6 (proxy en ubuntu12_32/):
   ├── XTest functions forwarded via dlsym
   └── ctor: dlopen liblumaforge.so

3. liblumaforge.so init (#[ctor]):
   ├── Panic hook → /tmp/steamcdp_proxy.log
   ├── Identifica proceso via /proc/self/cmdline
   ├── Solo ejecuta init en "steam" principal (no child processes)
   ├── Thread 1: stealth_kill_all_webhelpers() (lee /proc)
   ├── Thread 2: theme::export_theme_for_cef_hook() + ipc::start_ipc_server()
   ├── Thread 3: plugin_loader_linux::load_all_plugins() + lua_backend
   ├── Thread 4: bridge::start_bridge_server() (port 21775)
   ├── patch_steamwebhelper_script() — añade --remote-debugging-port
   └── Thread 6: hook_linux::start_cdp_injection_loop()

4. CDP injection loop (1s cycle):
   ├── Publica discovery (steam-cdp.json)
   ├── Exponential backoff: 100ms → 200ms → 400ms → 500ms
   ├── Connects to CDP (attempt ~3)
   ├── inject_all() — bridge proxy + plugins + theme patches
   └── Loop infinito:
       ├── Drain bridge queue every 1s
       ├── Recheck every 30s (all targets, store first)
       ├── URL change detection → immediate re-inject
       └── Detect new targets → inject

5. Steam CEF loads store.steampowered.com:
   ├── inject.js runs (via addScriptToEvaluateOnNewDocument)
   ├── bridge proxy intercepts fetch to 127.0.0.1:21775
   ├── Queue drain by CDP proxy (every 1s)
   ├── make_bridge_request: tries 21777 (luma-lite) first, 21775 (stub) fallback
   └── Response injected back → Promise resolves

6. luma-lite (si está corriendo en port 21777):
   ├── Bind port 21777 (21775 taken by CDP proxy stub)
   ├── Handle API requests from inject.js (via bridge proxy)
   └── Full functionality: depots, sources, downloads
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

### 1. Compilar CDP Proxy (32-bit)
```bash
cd ~/Codigo/lumaforge-cdp-proxy
CC_i686_unknown_linux_gnu="gcc -m32" OPENSSL_DIR=/usr OPENSSL_LIB_DIR=/usr/lib OPENSSL_INCLUDE_DIR=/usr/include \
  cargo build --release --target i686-unknown-linux-gnu
```

### 2. Deployear
```bash
cp target/i686-unknown-linux-gnu/release/liblumaforge.so \
   ~/.local/share/Steam/ubuntu12_32/
```

### 3. Lanzar Steam
```bash
# Steam carga libXtst.so.6 automáticamente
steam

# Verificar logs
tail -f /tmp/steamcdp_proxy.log
```

### 4. Verificar funcionamiento
```bash
# Buscar connected
grep "Connected to CDP" /tmp/steamcdp_proxy.log

# Buscar inyección
grep "Injected" /tmp/steamcdp_proxy.log

# Buscar bridge
grep "BRIDGE" /tmp/steamcdp_proxy.log

# Buscar URL change
grep "Store URL changed" /tmp/steamcdp_proxy.log
```

### 5. Probar LumaLite (pendiente de fix)
```bash
cd ~/Codigo/luma-lite/src-tauri
cargo check  # Debería compilar después del fix
cargo build --release
./target/release/luma-lite
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

### CDP Proxy (completado)
- [x] `hook/src/lib.rs` — libXtst.so.6 proxy with INIT_DONE guard + macro-based XTest
- [x] `hook_linux.rs` — CDP injection loop with exponential backoff
- [x] `hook_linux.rs` — URL change detection for immediate re-injection
- [x] `hook_linux.rs` — Bridge drain every 1s (was 5s)
- [x] `hook_linux.rs` — Bridge request priority: 21777 (luma-lite) first, 21775 (stub) fallback
- [x] `hook_linux.rs` — Recheck all targets every 30s (store first)
- [x] `injector.rs` — BRIDGE_PROXY_JS for mixed-content fix
- [x] `lib.rs` — Panic hook, stealth kill, plugin loading, bridge server
- [x] `lib.rs` — Desktop launch via libXtst proxy (no LD_PRELOAD needed)

### Pendiente
- [ ] **luma-lite: Fix compile** — `game_fix.rs:1043` winreg without cfg guard
- [ ] **luma-lite: Linux gaps** — `is_steam_running()`, `launch_steam_process()`, `set_start_with_system()`
- [ ] **luma-lite: IPC client** — Connect to `/tmp/lumalite_core.sock` from luma-lite
- [x] **Port conflict** — CDP proxy fallback to 21776 if 21775 is taken by luma-lite
- [x] **Bridge priority** — make_bridge_request tries 21777 first, then 21775
- [x] **Bridge drain** — Reduced from 5s to 1s for faster request processing
- [x] **CDP backoff** — Reduced from 200ms→1s to 100ms→500ms for faster connection
- [ ] `cdp_pipe.rs` — Integrar pipe CDP client (pv-adverb approach)
- [ ] `system_toggle.rs` — LD_AUDIT equivalent para Linux en luma-lite
- [ ] Testing completo: CDP injection, theme export, plugin loading, bridge proxy
- [ ] Packaging: `.deb` y/o AppImage para LumaLite

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
