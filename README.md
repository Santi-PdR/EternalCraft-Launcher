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

The home page can install the Minecraft 1.20.1 + Forge base into an app-managed instance directory. It verifies the Forge installer SHA-1, runs the installer with a validated Java 17 runtime, then validates the generated profile and checks the required game files. This base install is separate from any linked SKLauncher folder; it does not add EternalCraft mods. Microsoft sign-in uses the system browser, OAuth PKCE and a loopback callback, then verifies the Xbox Live and Minecraft profile flow. The app keeps access and refresh tokens only in process memory and forgets the session when the launcher closes. The registered public Microsoft desktop-app Client ID is configured in the Developer integrations panel before sign-in; it is public configuration, not a secret. Do not add a client secret. The first-run screen lets users sign in or browse without an account; gameplay still requires an entitled Microsoft account. Minecraft is launched only after a real pack is published and synced.

The mods page can import, activate and remove personal Forge JARs. Imported mods are stored in `mods/personales/` and linked or copied into Forge's directly scanned `mods/` root; it rejects JARs without Forge metadata, name collisions, and unsafe filenames. Forge library JARs marked `FMLModType: LIBRARY` are valid pack entries too. The official pack sync path is implemented for a published manifest: downloads are staged on disk, size and SHA-256 checked, verified as Forge mod JARs, and reconciled only against files recorded as official by the previous launcher sync. Personal files and untracked mods are preserved. No pack is currently published in the catalog; the Developer page can publish one after its GitHub App is configured and the real mod source is selected. See [the pack manifest contract](docs/pack-manifest.md).

Developer authorization uses a GitHub App configured by its public Client ID, GitHub's device flow, and repository permissions; the app checks write access to `Santi-PdR/EternalCraft-Launcher` and keeps the access token in memory only. Create/install an App with Device Flow enabled and only `Contents: Read and write` on this repository. Do not add an App private key, shared password, or client secret to the launcher. At startup the launcher fetches the series catalog from GitHub and falls back to the embedded catalog when offline. The Developer page scans only top-level JAR files, validates Forge archives and SHA-256, and displays each declared mod license. Publishing requires an acknowledgement tied to the exact reviewed source fingerprint; if any file changes, the source must be reviewed again. This is a review gate, not a legal determination: the publisher does not establish redistribution rights. It uploads to a draft release, verifies GitHub asset digests, publishes the release, then advances the series manifest and enables the catalog entry. Retries reuse matching release assets and replace stale assets only in an unpublished draft.

Forge installation progress and installer output are kept in a rotating local log under the app data directory and can be read from Support. Linux release builds use verified `.deb` and `.rpm` bundles; the Windows workflow builds an NSIS installer.

## Launcher updates and releases

The launcher checks its signed update channel at startup and shows an update action in the launcher window when a signed release is available; update controls are not part of Settings. In-app installation is enabled for Linux AppImage and Windows NSIS installations. Fedora RPM and Debian DEB installations can check the channel, but must be updated by installing the newer package because replacing system packages from inside the app would require elevated privileges. The updater verifies each artifact with the committed public key before installation.

To publish a launcher release:

1. Set `version` in `src-tauri/tauri.conf.json` and `package.json` to the same SemVer value and commit the change to `main`.
2. Create and push the matching tag, for example `launcher-v0.1.0`.
3. The `Launcher release` workflow runs checks, builds signed Linux AppImage/RPM/DEB and Windows NSIS installers, publishes the assets, and advances `latest.json` on the dedicated `launcher-updates` branch. The workflow must be exercised successfully on the first stable tag before describing signed self-updates as operational.

The repository Actions secret `TAURI_SIGNING_PRIVATE_KEY` is required for signed builds. Keep its value identical to the private key used for the public key committed in `src-tauri/tauri.conf.json`; never place the private key in the repository, an issue, or a release asset. The release workflow publishes only after both platform build jobs succeed. The first successful release creates the updater channel branch automatically.

## Design constraints

- No Electron; Tauri owns the native window and Rust owns filesystem/process operations.
- Series metadata is catalog data, not code branches for each season.
- User-selected game directories and active series are persisted under the OS app configuration directory.
- Existing SKLauncher SIEGE and Ghouls instance directories are detected as suggestions and never modified automatically.
- Public developer tools must not embed a shared password or GitHub credential. Publishing requires a separately authorized service.
