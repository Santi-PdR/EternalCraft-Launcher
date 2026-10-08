# EternalCraft Launcher

Native Minecraft launcher for EternalCraft, built with Tauri 2, Rust, Svelte and TypeScript.

## Development

Requirements: Node.js 22.12+, Rust stable, and the Tauri system libraries for your operating system. On Fedora, install the Tauri Linux prerequisites (`webkit2gtk4.1-devel`, `openssl-devel`, `curl`, `wget`, `file`, `libappindicator-gtk3-devel`, `librsvg2-devel`, `libxdo-devel`, `patchelf`).

```sh
npm ci
npm run tauri dev
```

The initial shell is data-driven and contains SIEGE and Ghouls Outbreak series entries. Pack distribution is intentionally marked unpublished until real manifests and release assets are added to this repository; it does not fabricate a playable pack.

## Design constraints

- No Electron; Tauri owns the native window and Rust owns filesystem/process operations.
- Series metadata is catalog data, not code branches for each season.
- User-selected game directories and active series are persisted under the OS app configuration directory.
- Existing SKLauncher SIEGE and Ghouls instance directories are detected as suggestions and never modified automatically.
- Public developer tools must not embed a shared password or GitHub credential. Publishing requires a separately authorized service.
