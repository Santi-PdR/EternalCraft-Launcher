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

Only flat `mods/*.jar` entries are accepted. Every download URL must point to an immutable asset in an EternalCraft Launcher GitHub release; arbitrary GitHub repositories, raw files, Modrinth, CurseForge and other mod catalogs are rejected. The downloader limits a mod to 512 MiB and a pack to 4 GiB, checks declared byte sizes and SHA-256, and validates each staged archive as a Forge mod JAR before changing the instance. A Forge mod must contain parseable `META-INF/mods.toml` with nonempty `modLoader` and `loaderVersion`, plus at least one unique `[[mods]]` entry whose `modId` follows Forge's ID format. `mods.toml` is limited to 256 KiB. Forge runtime libraries such as Kotlin for Forge are accepted only when their `META-INF/MANIFEST.MF` says `FMLModType: LIBRARY`; arbitrary JARs are rejected.

The launcher records the names and hashes it installed in its per-instance `.eternalcraft/<series>-official-pack.json` state file. On a later version, it updates and removes only those tracked official files. User-owned files in `mods/personales/` and untracked files in `mods/` are kept. If a filename collides with a personal or untracked mod, the sync stops before modifying the instance.

The repository remains unpublished until a developer selects a real source directory and completes a release. The Developer page verifies top-level JARs, displays each declared license, and checks declared Minecraft and Forge loader version ranges in `mods.toml` against the selected series. A declared incompatible JAR blocks publication; missing or unreadable compatibility metadata is surfaced for manual review. Publishing requires a confirmation tied to the exact source fingerprint so a changed JAR cannot bypass the review. This acknowledgement is not proof of redistribution rights; the developer must verify permissions and follow each license.

Publishing uses only the top-level JAR files in the developer-selected source folder. It does not query mod catalogs or accept external mod URLs. GitHub changes spaces and apostrophes to dots and removes brackets and parentheses in uploaded asset names; the preview shows the resulting name. The publisher rejects collisions after this normalization, rechecks each source file against its reviewed size and SHA-256, uploads to a draft EternalCraft release, verifies GitHub's reported digest, and writes the normalized asset name into the manifest. The developer must review each mod's license and confirm that EternalCraft has the rights needed to distribute the pack. Each installed file is checked by SHA-256 before the instance changes.

The publisher checks each GitHub-reported SHA-256, makes the verified release assets public before advancing the per-series manifest pointer, then marks the catalog entry available. This ordering leaves the previous complete pack usable if a publication is interrupted. A retry reuses verified assets and replaces stale assets only while the release is a draft; after publication it requires the exact same EternalCraft-hosted asset-name set and digests. The pointer advances only to a newer stable version, and a published version cannot be rewritten. Uploads resume between files, not inside a partially transferred file. The publisher does not create a playable pack until this complete flow succeeds.
