# EternalCraft Launcher — product contract

This file preserves the requested product concepts while the implementation is rebuilt from a clean codebase.

## Product structure

- One public launcher serves multiple EternalCraft series and future seasons. The series catalog is data-driven; SIEGE and Ghouls Outbreak are the first entries.
- Themes and user-selected backgrounds belong to appearance preferences. SIEGE can use a tactical siege identity; Ghouls can use a zombie-apocalypse identity inspired by Left 4 Dead. Do not generate artwork. Prefer user-provided assets or appropriately licensed remote artwork, with lightweight cached assets and a no-image fallback.
- The mods library must distinguish official pack files from user-owned personal mods. Official updates reconcile against the latest manifest, deleting obsolete official files. Personal files survive updates and remain separately identifiable.
- Developer tools are available in the same public application after authorization. A local UI gate is not a security boundary: do not embed a shared password, its verifier, GitHub credentials, or a signing key in the distributed binary. Authenticate against a protected HTTPS service with rate limits, short-lived sessions, and server-side authorization for publish operations.
- Publishing supports SIEGE and Ghouls independently. Developer selects the series and source folder; only intended mod files are included, not configs or unrelated instance state. Uploads should be content-addressed and resumable, publish a manifest/channel only after all referenced blobs are verified, and retain prior release metadata only as required for rollback—not stale local pack files.
- Launcher self-updates must be signed and replace the installed app cleanly. Temporary downloads and superseded binaries should be cleaned after a successful update without deleting user data.
- Native desktop app for Linux/Fedora and Windows. Do not use Electron. Never launch Minecraft during development smoke tests unless the user asks.

## Trust and data rules

- Never display an unpublished pack as playable. Do not invent mod counts, versions, release status, progress, or successful installs.
- Preserve existing user instance directories. Detection is read-only; the user explicitly chooses whether to link a directory.
- Keep account/session secrets out of exported settings, logs, diagnostics, and support bundles.

## Implementation status

- The native shell now adapts to narrow windows down to a 560 px minimum width; production frontend checks pass. Linux `.deb` and `.rpm` bundles were built locally, but were not installed or launched.
- Appearance preferences persist among series-accent, SIEGE, and Ghouls themes. Users may choose a local PNG/JPEG/WebP/GIF background up to 8 MiB; the launcher validates signatures, stores it under app config behind a narrowly scoped Tauri asset protocol, and removes a replaced background. No image generation or bundled artwork is used.
- Forge installation stage/output messages are persisted in a rotating local log, capped at 1 MiB plus one prior segment, and readable in the Support page. The Linux CI bundle list is limited to `.deb` and `.rpm`, matching the formats that built successfully here; Windows CI uses NSIS.
- The mods view reports JARs at the Forge `mods/` root and files stored under `mods/Oficiales/` or `mods/personales/`. Personal mods can be imported, activated and removed: the app validates the Forge archive and stores a copy under `mods/personales/`, then hard-links it into `mods/` or safely copies it there when hard links are unavailable. Official pack reconciliation is implemented, but neither series has a published manifest or assets, and developer-side publishing is still missing.
- Forge 1.20.x scans JAR files directly in the mods directory. Do not claim that nested category folders are active unless the launcher mirrors their managed files into the root with a tested cross-platform mechanism.
- The home page installs the vanilla 1.20.1 + catalog Forge profile into an app-managed directory using Java 17. It provisions Mojang's Java 17 runtime when the user has no compatible Java selected, verifies the Forge installer checksum, downloads/verifies the profile files and does not launch Minecraft. A network-dependent ignored smoke test is available to exercise the real installer without starting the game; it could not run in this sandbox because DNS/network access is unavailable. This is only the base game/loader, not a published EternalCraft pack.
- Microsoft sign-in now uses the operating-system browser, a local loopback redirect, PKCE state verification, and the existing Microsoft → Xbox Live/XSTS → Minecraft entitlement flow. The account refresh token and access token live only in memory and are discarded when the launcher closes; no account token is written into settings or logs. The user must configure EternalCraft's registered public desktop Client ID in Settings. The launch command refreshes the session, requires Java 17 and a valid Forge profile, and is only exposed for a series with a published, synced pack. This flow still needs Windows/Linux Rust CI and an actual Microsoft app registration/account to validate end to end; no Minecraft process was started during development.
- Official pack synchronization now has a strict per-series manifest contract. It stages downloads on disk, checks size and SHA-256, validates each archive as a Forge mod, refuses path traversal and personal/unmanaged filename collisions, tracks only official files, removes retired official files, and rolls back managed changes if activation fails. It never downloads configs or deletes untracked mods. No manifest exists yet under `packs/siege/` or `packs/ghouls-outbreak/`, so the catalog continues to mark both packs unpublished and the live network install flow remains unavailable until real pack assets are published.
- SIEGE and Ghouls still have no official pack manifest or published assets in this repository. Keep both series unavailable until those real inputs are provided and verified.
