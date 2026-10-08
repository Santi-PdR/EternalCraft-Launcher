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
