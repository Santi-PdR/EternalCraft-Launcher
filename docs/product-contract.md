# EternalCraft Launcher — product contract

This file preserves the requested product concepts while the implementation is rebuilt from a clean codebase.

## Product structure

- One public launcher serves multiple EternalCraft series and future seasons. The series catalog is data-driven; SIEGE and Ghouls Outbreak are the first entries.
- Themes and user-selected backgrounds belong to appearance preferences. SIEGE can use a tactical siege identity; Ghouls can use a zombie-apocalypse identity inspired by Left 4 Dead. Do not generate artwork. Prefer user-provided assets or appropriately licensed remote artwork, with lightweight cached assets and a no-image fallback.
- The mods library must distinguish official pack files from user-owned personal mods. Official updates reconcile against the latest manifest, deleting obsolete official files. Personal files survive updates and remain separately identifiable.
- Developer tools are available in the same public application only after authorization by a GitHub App installed on the EternalCraft-Launcher repository. A local UI gate is not a security boundary: do not embed a shared password, its verifier, GitHub credentials, app private key, or signing key in the distributed binary. GitHub's API enforces the installed app permissions and the user's repository write permission; request only Contents read/write, keep user tokens in process memory, and clear them on launcher exit/logout.
- Publishing supports SIEGE and Ghouls independently. Developer selects the series and source folder; only top-level Forge JARs are included, not configs or unrelated instance state. Each JAR is SHA-256 verified, uploaded to a draft release, and checked against GitHub's asset digest before the manifest is written and the release/catalog are published. Retries reuse already verified files and repair a draft; a failed individual upload restarts that file. Published release tags are immutable and changed content requires a new version.
- Launcher self-updates must be signed and replace the installed app cleanly. Temporary downloads and superseded binaries should be cleaned after a successful update without deleting user data.
- Native desktop app for Linux/Fedora and Windows. Do not use Electron. Never launch Minecraft during development smoke tests unless the user asks.

## Trust and data rules

- Never display an unpublished pack as playable. Do not invent mod counts, versions, release status, progress, or successful installs.
- Preserve existing user instance directories. Detection is read-only; the user explicitly chooses whether to link a directory.
- Keep account/session secrets out of exported settings, logs, diagnostics, and support bundles.

## Implementation status

- The native Tauri shell supports Linux/Fedora and Windows, a 560 px minimum width, a first-run account/install flow, and persistent light, series-accent, SIEGE, and Ghouls themes. Local backgrounds are size/type validated and stored under app config; no generated or bundled artwork is used.
- The Developer page handles GitHub Device Flow and pack publication. It scans only top-level Forge JARs, verifies source hashes, uploads resumably to a draft release, verifies GitHub SHA-256 digests, and publishes the manifest/catalog. Published release tags are immutable; the per-series manifest is a moving pointer that advances only after a complete release is public. Retries require the exact same asset-name set and hashes, never rewrite a manifest at the same version, and reject attempts to move the pointer backward. The full publisher has not yet completed a real release against this account.
- The mod library imports personal mods into `mods/personales/` and links/copies active JARs into Forge's root. Official pack sync stages, verifies and reconciles only tracked official files; it preserves personal and untracked files. Forge 1.20.x scans JARs directly in `mods/`, so the managed subfolders are storage organization, not Forge scan paths.
- The home page can install the 1.20.1 + Forge base into an app-managed instance and provision Mojang Java 17 if needed. Microsoft auth uses system-browser OAuth PKCE, loopback validation, Xbox Live/XSTS and Minecraft entitlement checks. Tokens remain in process memory. The launch command does not start until a legitimate Minecraft account, valid Java/Forge installation and a published/synced pack are available.
- Both series remain unpublished in the repository catalog: there are no manifests/assets in this repo yet. The local SIEGE/Ghouls mod folders are source candidates, not proof of a release. Do not mark either series playable until a real release has been published and a clean install/update has been verified.
- Linux `.deb`/`.rpm` and Windows NSIS native builds, Rust tests, and startup smoke tests pass in GitHub Actions. These smoke tests only start the launcher and never start Minecraft. A first official stable release, Microsoft login with the registered app, and a real end-to-end pack install/update remain to be verified.
- Signed updater support is configured for Linux AppImage and Windows NSIS. Fedora RPM and Debian DEB require package installation for updates. The first signed release still needs to prove that the Actions private key matches the public key committed in Tauri config and that the update channel installs a newer build successfully.
