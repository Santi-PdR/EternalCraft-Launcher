# EternalCraft Launcher

Native Minecraft launcher for EternalCraft, built with Tauri 2, Rust, Svelte and TypeScript.

## Development

Requirements: Node.js 22.12+, Rust stable, and the Tauri system libraries for your operating system. On Fedora, install the Tauri Linux prerequisites (`webkit2gtk4.1-devel`, `openssl-devel`, `curl`, `wget`, `file`, `libappindicator-gtk3-devel`, `librsvg2-devel`, `libxdo-devel`, `patchelf`).

```sh
npm ci
npm run tauri dev
```

The initial shell is data-driven and contains SIEGE and Ghouls Outbreak series entries. Pack distribution is intentionally marked unpublished until real manifests and release assets are added to this repository; it does not fabricate a playable pack.

The settings page detects Java installations, prefers Java 17, and lets the user select a Java 17 executable explicitly. The selection is persisted locally. If Java 17 is unavailable, installing the base provisions Mojang's Java 17 runtime inside the app-managed instance.

Appearance preferences persist per installation: use the active series accent, a SIEGE theme, or a Ghouls theme. A user-selected PNG, JPEG, WebP, or GIF background is copied to app configuration storage (8 MiB maximum); no artwork is bundled or fetched.

The home page can install the Minecraft 1.20.1 + Forge base into an app-managed instance directory. It verifies the Forge installer SHA-1, runs the installer with a validated Java 17 runtime, then validates the generated profile and checks the required game files. This base install is separate from any linked SKLauncher folder; it does not add EternalCraft mods, authenticate an account, or launch the game. An ignored integration smoke test can exercise the official download/Forge installer flow in a temporary directory and never launches Minecraft.

The mods page can import, activate and remove personal Forge JARs. Imported mods are stored in `mods/personales/` and linked or copied into Forge's directly scanned `mods/` root; it refuses non-mod JARs, name collisions, and unsafe filenames. The official pack sync path is implemented for a published manifest: downloads are staged on disk, size and SHA-256 checked, verified as Forge mod JARs, and reconciled only against files recorded as official by the previous launcher sync. Personal files and untracked mods are preserved. No pack is currently published in the catalog, and developer publishing is not implemented yet. See [the pack manifest contract](docs/pack-manifest.md).

Forge installation progress and installer output are kept in a rotating local log under the app data directory and can be read from Support. Linux release builds use verified `.deb` and `.rpm` bundles; the Windows workflow builds an NSIS installer.

## Design constraints

- No Electron; Tauri owns the native window and Rust owns filesystem/process operations.
- Series metadata is catalog data, not code branches for each season.
- User-selected game directories and active series are persisted under the OS app configuration directory.
- Existing SKLauncher SIEGE and Ghouls instance directories are detected as suggestions and never modified automatically.
- Public developer tools must not embed a shared password or GitHub credential. Publishing requires a separately authorized service.
