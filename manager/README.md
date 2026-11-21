# ReShade Manager for Windows

The portable manager shares the ReShade runtime and content catalogs with upstream. The original setup application stays in the solution as a fallback.

## Using the manager

Download the Windows manager artifact from this fork's **ReShade Manager Windows** Actions workflow. Run `reshade-manager.exe` and select an installed game. Review the executable and graphics API before choosing shaders or add-ons. Setup downloads the corresponding ReShade runtime from reshade.me and does not start the game.

The app stores settings and its library under `%LOCALAPPDATA%\ReShade Manager`. `RESHADE_MANAGER_DATA` can point to a separate directory for portable storage, testing, or diagnostics. Files installed by the manager are tracked in a receipt alongside the game. Each game retains its own ReShade.ini and presets. Uninstall removes a recognized ReShade wrapper and unmodified files recorded in its receipt. Existing configurations, presets and custom resources stay in place.

Use **Open legacy setup** for Vulkan's shared Windows layer and for updates of imported installations whose original runtime variant cannot be determined. It opens a setup executable you choose. The official website supplies standard and add-on-enabled setup executables. The fork's compatibility job also builds a legacy fallback from source.

## Compatibility and storage

- Game discovery uses Steam ACF and library metadata, Epic installation manifests and GOG registry entries. Game directories are bounded, and custom paths and manually added executables persist. A store's primary executable may be ambiguous. Add the exact executable manually when needed.
- Analysis reads PE machine type, import metadata, an Unreal bootstrap resource and upstream Compatibility.ini. Manual graphics selection is available except when the compatibility catalog specifies a forced wrapper. Online access is needed once to obtain compatibility data.
- Direct3D and OpenGL use the original ReShade DLL filename conventions. The DLL is extracted from the official versioned distribution. The manager does not implement a renderer or shader compiler.
- Downloaded shader ZIPs are keyed by repository URL and resolved commit, and checked against a stored SHA-256 on reuse. Add-ons come from the upstream catalog with release and architecture validation. Publisher digests are checked when GitHub supplies them. An add-on can have separate upstream versions for x86 and x64.
- Shader packages keep ordinary includes and textures from their selected package. Package layouts outside the shader and texture conventions require inspection before support can be claimed. Selected effects and included assets retain their package-relative paths. Shared storage publishes content into versioned directories and leaves game presets local. Clearing the archive cache leaves live shared resources alone.
- Steam artwork uses local launcher data or an official Steam CDN URL keyed to the app ID. Epic and GOG use available local metadata. The executable icon is used when artwork is missing.
- No telemetry, background service, artwork bundle or shader bundle is shipped.

## Validation

The Windows workflow builds ReShade x86 and x64 and its legacy setup, then builds and tests the Rust and Slint executable in a separate job. Tests use synthetic launcher manifests and PE fixtures and exercise installation, rollback, offline package reuse, and preservation of independent presets. The Slint software renderer can capture fixture screenshots without a commercial game. An optional startup sample records startup to the event loop, working set, and private memory on the Windows runner when an interactive desktop is available.

Real-game loading, renderer output, driver interactions, layer registration and game anti-cheat behavior require separate validation. CI fixture tests exercise file and settings operations without loading a DLL into any game.
