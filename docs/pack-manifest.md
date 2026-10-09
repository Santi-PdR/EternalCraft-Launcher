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

Only flat `mods/*.jar` entries are accepted. The downloader allows HTTPS URLs hosted on GitHub or `raw.githubusercontent.com`, limits a mod to 512 MiB and a pack to 4 GiB, and checks declared byte sizes and SHA-256, and requires each archive to contain either `META-INF/mods.toml` or a Forge manifest (`META-INF/MANIFEST.MF`) marked `FMLModType: LIBRARY` before changing the instance. This includes Forge runtime libraries such as Kotlin for Forge while still rejecting arbitrary JARs.

The launcher records the names and hashes it installed in its per-instance `.eternalcraft/<series>-official-pack.json` state file. On a later version, it updates and removes only those tracked official files. User-owned files in `mods/personales/` and untracked files in `mods/` are kept. If a filename collides with a personal or untracked mod, the sync stops before modifying the instance.

The repository remains unpublished until a developer selects a real source directory and completes a release. The Developer page now verifies top-level JARs, uploads them into a draft GitHub release, checks each GitHub-reported SHA-256, writes the manifest, publishes the release, and marks that series available in the catalog. A retry reuses verified assets and replaces stale assets in a draft. Uploads resume between files, not inside a partially transferred file. A release whose tag is already published is immutable: changed content requires a new version. The publisher does not create a playable pack until this complete flow succeeds.
