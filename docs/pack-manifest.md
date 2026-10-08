# Official pack manifest contract

The public launcher reads one file per series from:

`packs/<series-id>/manifest.json`

The series id must exist in `resources/series/catalog.json` (`siege` and `ghouls-outbreak` today). A series stays `unpublished` until its real files and manifest are committed and the catalog points to the matching Minecraft and Forge versions.

```json
{
  "schemaVersion": 1,
  "seriesId": "siege",
  "version": "1.0.0",
  "minecraftVersion": "1.20.1",
  "loader": "forge",
  "loaderVersion": "47.4.10",
  "files": [
    {
      "path": "mods/example-mod.jar",
      "url": "https://github.com/Santi-PdR/EternalCraft-Launcher/releases/download/siege-v1.0.0/example-mod.jar",
      "sizeBytes": 123456,
      "sha256": "<64 hexadecimal characters>"
    }
  ]
}
```

Only flat `mods/*.jar` entries are accepted. The downloader allows HTTPS URLs hosted on GitHub or `raw.githubusercontent.com`, limits a mod to 512 MiB and a pack to 4 GiB, and checks declared byte sizes, SHA-256, and the presence of `META-INF/mods.toml` before changing the instance.

The launcher records the names and hashes it installed in its per-instance `.eternalcraft/<series>-official-pack.json` state file. On a later version, it updates and removes only those tracked official files. User-owned files in `mods/personales/` and untracked files in `mods/` are kept. If a filename collides with a personal or untracked mod, the sync stops before modifying the instance.

The manifest and binaries do not exist yet in the repository. This document describes the consumer contract; it is not a release and does not mark either series playable. Developer-side manifest generation, resumable upload, publication authorization, and signed release metadata remain separate work.
