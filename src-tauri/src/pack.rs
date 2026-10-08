use atomic_write_file::AtomicWriteFile;
use mc_launcher_core::net::http;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

const MANIFEST_LIMIT: u64 = 2 * 1024 * 1024;
const MOD_LIMIT: u64 = 512 * 1024 * 1024;
const PACK_LIMIT: u64 = 4 * 1024 * 1024 * 1024;
static PACK_SYNC_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct PackManifest {
    schema_version: u32,
    series_id: String,
    version: String,
    minecraft_version: String,
    loader: String,
    loader_version: String,
    files: Vec<PackFile>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct PackFile {
    path: String,
    url: String,
    size_bytes: u64,
    sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct PackState {
    schema_version: u32,
    series_id: String,
    version: String,
    files: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PackSyncResult {
    pub series_id: String,
    pub version: String,
    pub downloaded_files: usize,
    pub removed_files: usize,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PackProgress {
    pub series_id: String,
    pub completed_files: usize,
    pub total_files: usize,
    pub message: String,
}

fn valid_file_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 180
        && name.to_ascii_lowercase().ends_with(".jar")
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'+'))
}

fn validate_manifest(manifest: &PackManifest, expected_series: &str) -> Result<(), String> {
    if manifest.schema_version != 1 {
        return Err(format!(
            "Versión de manifiesto no compatible: {}",
            manifest.schema_version
        ));
    }
    if manifest.series_id != expected_series {
        return Err("El manifiesto remoto corresponde a otra serie".into());
    }
    if manifest.version.trim().is_empty()
        || manifest.version.len() > 80
        || manifest.minecraft_version.trim().is_empty()
        || manifest.loader.trim().is_empty()
        || manifest.loader_version.trim().is_empty()
    {
        return Err("El manifiesto tiene metadatos incompletos".into());
    }
    if manifest.files.len() > 5000 {
        return Err("El manifiesto excede el máximo de 5000 mods".into());
    }

    let mut names = BTreeSet::new();
    let mut total = 0u64;
    for file in &manifest.files {
        let Some(name) = file.path.strip_prefix("mods/") else {
            return Err(format!("Ruta fuera de mods/ rechazada: {}", file.path));
        };
        if !valid_file_name(name) || file.path.matches('/').count() != 1 {
            return Err(format!("Ruta de mod no válida: {}", file.path));
        }
        if !names.insert(name.to_ascii_lowercase()) {
            return Err(format!("El manifiesto repite el nombre de archivo {name}"));
        }
        if file.size_bytes == 0 || file.size_bytes > MOD_LIMIT {
            return Err(format!("Tamaño no válido para {name}"));
        }
        total = total
            .checked_add(file.size_bytes)
            .ok_or_else(|| "El tamaño total del manifiesto excede el límite".to_string())?;
        if file.sha256.len() != 64 || !file.sha256.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!("SHA-256 no válido para {name}"));
        }
        if !(file.url.starts_with("https://github.com/")
            || file.url.starts_with("https://raw.githubusercontent.com/"))
        {
            return Err(format!("Origen de descarga no permitido para {name}"));
        }
    }
    if total > PACK_LIMIT {
        return Err("El tamaño total del pack supera 4 GiB".into());
    }
    Ok(())
}

#[cfg(test)]
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn digest_file(path: &Path) -> Result<String, String> {
    let mut file = fs::File::open(path).map_err(|error| {
        format!(
            "No se pudo abrir {} para verificarlo: {error}",
            path.display()
        )
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| format!("No se pudo verificar {}: {error}", path.display()))?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn check_normal_directory(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(()),
        Ok(_) => Err(format!(
            "La ruta administrada no es un directorio normal: {}",
            path.display()
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("No se pudo validar {}: {error}", path.display())),
    }
}

fn check_normal_file_if_present(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(()),
        Ok(_) => Err(format!(
            "La ruta administrada no es un archivo normal: {}",
            path.display()
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("No se pudo validar {}: {error}", path.display())),
    }
}

fn remove_normal_file_if_present(path: &Path) -> Result<(), String> {
    check_normal_file_if_present(path)?;
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("No se pudo retirar {}: {error}", path.display())),
    }
}

fn has_name_case_insensitive(directory: &Path, name: &str) -> Result<bool, String> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => {
            return Err(format!(
                "No se pudo revisar {}: {error}",
                directory.display()
            ))
        }
    };
    for entry in entries {
        let entry = entry.map_err(|error| format!("No se pudo revisar un archivo: {error}"))?;
        if entry
            .file_name()
            .to_string_lossy()
            .eq_ignore_ascii_case(name)
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn load_previous_state(path: &Path, series_id: &str) -> Result<Option<PackState>, String> {
    match fs::read(path) {
        Ok(bytes) => {
            let state: PackState = serde_json::from_slice(&bytes)
                .map_err(|error| format!("El registro del pack está dañado: {error}"))?;
            if state.schema_version != 1 || state.series_id != series_id {
                return Err("El registro guardado no corresponde a esta serie".into());
            }
            for (name, hash) in &state.files {
                if !valid_file_name(name)
                    || hash.len() != 64
                    || !hash.bytes().all(|c| c.is_ascii_hexdigit())
                {
                    return Err("El registro guardado contiene una ruta o hash inválido".into());
                }
            }
            Ok(Some(state))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("No se pudo leer el registro del pack: {error}")),
    }
}

fn state_path(game_dir: &Path, series_id: &str) -> PathBuf {
    game_dir
        .join(".eternalcraft")
        .join(format!("{series_id}-official-pack.json"))
}

fn files_to_download(game_dir: &Path, manifest: &PackManifest) -> Result<Vec<PackFile>, String> {
    let state_path = state_path(game_dir, &manifest.series_id);
    check_normal_directory(&game_dir.join(".eternalcraft"))?;
    check_normal_file_if_present(&state_path)?;
    let previous = load_previous_state(&state_path, &manifest.series_id)?;
    let Some(previous) = previous else {
        return Ok(manifest.files.clone());
    };
    let mods = game_dir.join("mods");
    let officials = mods.join("Oficiales");
    check_normal_directory(&mods)?;
    check_normal_directory(&officials)?;
    let mut downloads = Vec::new();
    for file in &manifest.files {
        let name = file.path.strip_prefix("mods/").unwrap();
        if previous.files.get(name) != Some(&file.sha256.to_ascii_lowercase()) {
            downloads.push(file.clone());
            continue;
        }
        let official_path = officials.join(name);
        check_normal_file_if_present(&official_path)?;
        let is_current = match fs::metadata(&official_path) {
            Ok(metadata) if metadata.is_file() && metadata.len() == file.size_bytes => {
                digest_file(&official_path)? == file.sha256.to_ascii_lowercase()
            }
            Ok(_) => false,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(format!("No se pudo revisar el mod oficial {name}: {error}")),
        };
        if !is_current {
            downloads.push(file.clone());
        }
    }
    Ok(downloads)
}

fn atomic_save_state(path: &Path, state: &PackState) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(state).map_err(|error| error.to_string())?;
    let mut file = AtomicWriteFile::options()
        .open(path)
        .map_err(|error| format!("No se pudo preparar el registro del pack: {error}"))?;
    file.write_all(&bytes)
        .map_err(|error| format!("No se pudo escribir el registro del pack: {error}"))?;
    file.commit()
        .map_err(|error| format!("No se pudo confirmar el registro del pack: {error}"))
}

fn restore_snapshot(
    mods_dir: &Path,
    officials: &Path,
    state_path: &Path,
    staging: &Path,
    names: &BTreeSet<String>,
    old_state: Option<&PackState>,
    old_state_bytes: Option<&[u8]>,
) {
    for name in names {
        let _ = fs::remove_file(mods_dir.join(name));
        let _ = fs::remove_file(officials.join(name));
    }
    if let Some(state) = old_state {
        for name in state.files.keys() {
            let backup = staging.join("backup").join(name);
            if backup.is_file() {
                let _ = fs::copy(&backup, officials.join(name));
                let _ = fs::hard_link(officials.join(name), mods_dir.join(name))
                    .or_else(|_| fs::copy(officials.join(name), mods_dir.join(name)).map(|_| ()));
            }
        }
    }
    match old_state_bytes {
        Some(bytes) => {
            let _ = fs::write(state_path, bytes);
        }
        None => {
            let _ = fs::remove_file(state_path);
        }
    }
}

fn reconcile_staged(
    game_dir: &Path,
    manifest: &PackManifest,
    staging: &Path,
) -> Result<PackSyncResult, String> {
    validate_manifest(manifest, &manifest.series_id)?;
    let mods_dir = game_dir.join("mods");
    let officials = mods_dir.join("Oficiales");
    let personal = mods_dir.join("personales");
    check_normal_directory(&mods_dir)?;
    check_normal_directory(&officials)?;
    check_normal_directory(&personal)?;
    fs::create_dir_all(&mods_dir).map_err(|error| format!("No se pudo crear mods/: {error}"))?;
    fs::create_dir_all(&officials)
        .map_err(|error| format!("No se pudo crear mods/Oficiales/: {error}"))?;

    let state_path = state_path(game_dir, &manifest.series_id);
    check_normal_directory(&game_dir.join(".eternalcraft"))?;
    check_normal_file_if_present(&state_path)?;
    let old_state = load_previous_state(&state_path, &manifest.series_id)?;
    let old_state_bytes = fs::read(&state_path).ok();
    let old_names = old_state
        .as_ref()
        .map(|state| {
            state
                .files
                .keys()
                .map(|n| n.to_ascii_lowercase())
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();
    for file in &manifest.files {
        let name = file.path.strip_prefix("mods/").unwrap();
        let folded = name.to_ascii_lowercase();
        if has_name_case_insensitive(&personal, name)? {
            return Err(format!(
                "El mod oficial {name} colisiona con uno personal; no se modificó el pack"
            ));
        }
        let root_path = mods_dir.join(name);
        let official_path = officials.join(name);
        check_normal_file_if_present(&root_path)?;
        check_normal_file_if_present(&official_path)?;
        if has_name_case_insensitive(&mods_dir, name)? && !old_names.contains(&folded) {
            return Err(format!("El archivo {name} ya existe en mods/ y no está registrado como oficial; no se sobrescribió"));
        }
        if has_name_case_insensitive(&officials, name)? && !old_names.contains(&folded) {
            return Err(format!("El archivo {name} ya existe en mods/Oficiales/ y no está registrado como oficial; no se sobrescribió"));
        }
    }

    let result = (|| {
        let mut new_state = PackState {
            schema_version: 1,
            series_id: manifest.series_id.clone(),
            version: manifest.version.clone(),
            files: BTreeMap::new(),
        };
        for file in &manifest.files {
            let name = file.path.strip_prefix("mods/").unwrap();
            let staged = staging.join(name);
            let official = officials.join(name);
            let source = if staged.is_file() { &staged } else { &official };
            check_normal_file_if_present(source)?;
            let size = fs::metadata(source)
                .map_err(|error| format!("Falta la descarga verificada de {name}: {error}"))?
                .len();
            if size != file.size_bytes || digest_file(source)? != file.sha256.to_ascii_lowercase() {
                return Err(format!(
                    "El tamaño o SHA-256 de {name} no coincide; la instancia no se modificó"
                ));
            }
            super::validate_forge_mod_archive(source)?;
            new_state
                .files
                .insert(name.to_string(), file.sha256.to_ascii_lowercase());
        }
        let downloaded_files = manifest
            .files
            .iter()
            .filter(|file| {
                staging
                    .join(file.path.strip_prefix("mods/").unwrap())
                    .is_file()
            })
            .count();

        let backup_dir = staging.join("backup");
        fs::create_dir(&backup_dir).map_err(|error| error.to_string())?;
        if let Some(state) = &old_state {
            for name in state.files.keys() {
                let official = officials.join(name);
                let root = mods_dir.join(name);
                check_normal_file_if_present(&official)?;
                check_normal_file_if_present(&root)?;
                if official.is_file() {
                    fs::copy(&official, backup_dir.join(name))
                        .map_err(|error| error.to_string())?;
                } else if root.is_file() {
                    fs::copy(&root, backup_dir.join(name)).map_err(|error| error.to_string())?;
                }
            }
        }

        let state_parent = state_path.parent().unwrap();
        fs::create_dir_all(state_parent).map_err(|error| error.to_string())?;
        let transaction_names = old_state
            .as_ref()
            .into_iter()
            .flat_map(|state| state.files.keys().cloned())
            .chain(new_state.files.keys().cloned())
            .collect::<BTreeSet<_>>();

        let apply = (|| {
            for old in old_state
                .as_ref()
                .into_iter()
                .flat_map(|state| state.files.keys())
            {
                if !new_state.files.contains_key(old) {
                    remove_normal_file_if_present(&mods_dir.join(old))?;
                    remove_normal_file_if_present(&officials.join(old))?;
                }
            }
            for name in new_state.files.keys() {
                let staged = staging.join(name);
                let official = officials.join(name);
                let root = mods_dir.join(name);
                if staged.is_file() {
                    remove_normal_file_if_present(&official)?;
                    fs::rename(&staged, &official).map_err(|error| {
                        format!("No se pudo instalar el mod oficial {name}: {error}")
                    })?;
                }
                remove_normal_file_if_present(&root)?;
                fs::hard_link(&official, &root)
                    .or_else(|_| fs::copy(&official, &root).map(|_| ()))
                    .map_err(|error| {
                        format!("No se pudo activar el mod oficial {name}: {error}")
                    })?;
            }
            atomic_save_state(&state_path, &new_state)
        })();
        if let Err(error) = apply {
            restore_snapshot(
                &mods_dir,
                &officials,
                &state_path,
                &staging,
                &transaction_names,
                old_state.as_ref(),
                old_state_bytes.as_deref(),
            );
            return Err(error);
        }
        Ok(PackSyncResult {
            series_id: manifest.series_id.clone(),
            version: manifest.version.clone(),
            downloaded_files,
            removed_files: old_state
                .as_ref()
                .map(|state| {
                    state
                        .files
                        .keys()
                        .filter(|name| !new_state.files.contains_key(*name))
                        .count()
                })
                .unwrap_or(0),
        })
    })();
    result
}

pub(super) fn sync_pack(
    game_dir: &Path,
    series_id: &str,
    minecraft_version: &str,
    loader: &str,
    loader_version: &str,
    mut report_progress: impl FnMut(PackProgress),
) -> Result<PackSyncResult, String> {
    let _guard = PACK_SYNC_LOCK
        .lock()
        .map_err(|_| "El bloqueo de actualización del pack falló".to_string())?;
    let url = format!(
        "https://raw.githubusercontent.com/Santi-PdR/EternalCraft-Launcher/main/packs/{series_id}/manifest.json"
    );
    let client = http::client().map_err(|error| error.to_string())?;
    let response = client
        .get(&url)
        .send()
        .and_then(|response| response.error_for_status())
        .map_err(|error| format!("No se pudo consultar el manifiesto oficial: {error}"))?;
    if response
        .content_length()
        .is_some_and(|length| length > MANIFEST_LIMIT)
    {
        return Err("El manifiesto oficial supera el tamaño permitido".into());
    }
    let mut manifest_bytes = Vec::new();
    response
        .take(MANIFEST_LIMIT + 1)
        .read_to_end(&mut manifest_bytes)
        .map_err(|error| format!("No se pudo leer el manifiesto oficial: {error}"))?;
    if manifest_bytes.len() as u64 > MANIFEST_LIMIT {
        return Err("El manifiesto oficial supera el tamaño permitido".into());
    }
    let manifest: PackManifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|error| format!("El manifiesto oficial no es válido: {error}"))?;
    validate_manifest(&manifest, series_id)?;
    if manifest.minecraft_version != minecraft_version
        || !manifest.loader.eq_ignore_ascii_case(loader)
        || manifest.loader_version != loader_version
    {
        return Err(
            "El manifiesto no coincide con las versiones de Minecraft y Forge del catálogo".into(),
        );
    }
    let downloads = files_to_download(game_dir, &manifest)?;
    report_progress(PackProgress {
        series_id: series_id.into(),
        completed_files: 0,
        total_files: downloads.len(),
        message: format!(
            "Manifiesto {} verificado; preparando descargas",
            manifest.version
        ),
    });

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let staging = game_dir.join(format!(".eternalcraft-pack-{}-{nonce}", std::process::id()));
    fs::create_dir_all(&staging)
        .map_err(|error| format!("No se pudo preparar la actualización: {error}"))?;
    let result = (|| {
        for (index, file) in downloads.iter().enumerate() {
            let name = file.path.strip_prefix("mods/").unwrap();
            let response = client
                .get(&file.url)
                .send()
                .and_then(|response| response.error_for_status())
                .map_err(|error| format!("No se pudo descargar {name}: {error}"))?;
            if response
                .content_length()
                .is_some_and(|length| length != file.size_bytes)
            {
                return Err(format!(
                    "El servidor informó un tamaño inesperado para {name}"
                ));
            }
            let staged = staging.join(name);
            let mut output = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&staged)
                .map_err(|error| format!("No se pudo preparar la descarga de {name}: {error}"))?;
            let copied = std::io::copy(&mut response.take(file.size_bytes + 1), &mut output)
                .map_err(|error| format!("La descarga de {name} falló: {error}"))?;
            output
                .sync_all()
                .map_err(|error| format!("No se pudo confirmar la descarga de {name}: {error}"))?;
            if copied != file.size_bytes
                || digest_file(&staged)? != file.sha256.to_ascii_lowercase()
            {
                return Err(format!(
                    "La descarga de {name} no pasó la verificación SHA-256"
                ));
            }
            report_progress(PackProgress {
                series_id: series_id.into(),
                completed_files: index + 1,
                total_files: downloads.len(),
                message: format!("Verificado {}/{} · {name}", index + 1, downloads.len()),
            });
        }
        report_progress(PackProgress {
            series_id: series_id.into(),
            completed_files: downloads.len(),
            total_files: downloads.len(),
            message: "Instalando mods y retirando los oficiales obsoletos".into(),
        });
        reconcile_staged(game_dir, &manifest, &staging)
    })();
    let _ = fs::remove_dir_all(&staging);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::{write::SimpleFileOptions, ZipWriter};

    fn manifest(series_id: &str, files: &[(&str, &[u8])]) -> PackManifest {
        PackManifest {
            schema_version: 1,
            series_id: series_id.into(),
            version: "1.0.0".into(),
            minecraft_version: "1.20.1".into(),
            loader: "forge".into(),
            loader_version: "47.4.10".into(),
            files: files
                .iter()
                .map(|(name, bytes)| PackFile {
                    path: format!("mods/{name}"),
                    url: "https://github.com/Santi-PdR/EternalCraft-Launcher/releases/download/test/mod.jar".into(),
                    size_bytes: bytes.len() as u64,
                    sha256: digest(bytes),
                })
                .collect(),
        }
    }

    fn temp_dir() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "eternalcraft-pack-test-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    fn mod_jar(mod_id: &str) -> Vec<u8> {
        let mut archive = ZipWriter::new(std::io::Cursor::new(Vec::new()));
        archive
            .start_file("META-INF/mods.toml", SimpleFileOptions::default())
            .unwrap();
        write!(
            archive,
            "modLoader=\"javafml\"\n[[mods]]\nmodId=\"{mod_id}\"\n"
        )
        .unwrap();
        archive.finish().unwrap().into_inner()
    }

    #[test]
    fn rejects_paths_duplicates_untrusted_hosts_and_wrong_series() {
        let valid = b"jar bytes";
        let mut value = manifest("siege", &[("first.jar", valid)]);
        validate_manifest(&value, "siege").unwrap();
        value.files[0].path = "mods/../outside.jar".into();
        assert!(validate_manifest(&value, "siege").is_err());
        value = manifest("siege", &[("same.jar", valid), ("SAME.jar", valid)]);
        assert!(validate_manifest(&value, "siege").is_err());
        value = manifest("siege", &[("good.jar", valid)]);
        value.files[0].url = "https://example.invalid/mod.jar".into();
        assert!(validate_manifest(&value, "siege").is_err());
        assert!(validate_manifest(&value, "ghouls").is_err());
    }

    #[test]
    fn reconciles_official_changes_removes_retired_mods_and_preserves_personal_mods() {
        let game = temp_dir();
        let mods = game.join("mods");
        let personal = mods.join("personales");
        fs::create_dir_all(&personal).unwrap();
        fs::write(personal.join("custom.jar"), b"personal").unwrap();
        let old = mod_jar("old");
        let same = mod_jar("kept");
        let first = manifest("siege", &[("old.jar", &old), ("kept.jar", &same)]);
        let stage1 = game.join("stage1");
        fs::create_dir(&stage1).unwrap();
        fs::write(stage1.join("old.jar"), old).unwrap();
        fs::write(stage1.join("kept.jar"), same).unwrap();
        reconcile_staged(&game, &first, &stage1).unwrap();
        fs::remove_dir_all(&stage1).unwrap();
        assert!(files_to_download(&game, &first).unwrap().is_empty());
        let updated = mod_jar("kept");
        let new = mod_jar("new");
        let second = manifest("siege", &[("kept.jar", &updated), ("new.jar", &new)]);
        assert_eq!(
            files_to_download(&game, &second)
                .unwrap()
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            vec!["mods/new.jar"]
        );
        let stage2 = game.join("stage2");
        fs::create_dir(&stage2).unwrap();
        fs::write(stage2.join("new.jar"), new).unwrap();
        let result = reconcile_staged(&game, &second, &stage2).unwrap();
        assert_eq!(result.removed_files, 1);
        assert_eq!(result.downloaded_files, 1);
        assert!(!mods.join("old.jar").exists());
        assert!(!mods.join("Oficiales/old.jar").exists());
        assert_eq!(fs::read(mods.join("kept.jar")).unwrap(), updated);
        assert!(mods.join("Oficiales/new.jar").is_file());
        assert_eq!(fs::read(personal.join("custom.jar")).unwrap(), b"personal");
        fs::remove_dir_all(&stage2).unwrap();

        let changed_kept = mod_jar("kept-v2");
        let third = manifest("siege", &[("kept.jar", &changed_kept)]);
        assert_eq!(
            files_to_download(&game, &third)
                .unwrap()
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>(),
            vec!["mods/kept.jar"]
        );
        let stage3 = game.join("stage3");
        fs::create_dir(&stage3).unwrap();
        fs::write(stage3.join("kept.jar"), &changed_kept).unwrap();
        let result = reconcile_staged(&game, &third, &stage3).unwrap();
        assert_eq!(result.downloaded_files, 1);
        assert_eq!(result.removed_files, 1);
        assert!(!mods.join("new.jar").exists());
        assert_eq!(fs::read(mods.join("kept.jar")).unwrap(), changed_kept);
        fs::remove_dir_all(&stage3).unwrap();
        fs::remove_dir_all(game).unwrap();
    }

    #[test]
    fn empty_official_release_removes_only_tracked_official_files() {
        let game = temp_dir();
        let mods = game.join("mods");
        fs::create_dir_all(&mods).unwrap();
        let old = mod_jar("old");
        let first = manifest("siege", &[("old.jar", &old)]);
        let stage = game.join("stage1");
        fs::create_dir(&stage).unwrap();
        fs::write(stage.join("old.jar"), old).unwrap();
        reconcile_staged(&game, &first, &stage).unwrap();
        fs::remove_dir_all(&stage).unwrap();

        fs::write(mods.join("unmanaged.jar"), b"leave me alone").unwrap();
        let empty = manifest("siege", &[]);
        let stage = game.join("stage2");
        fs::create_dir(&stage).unwrap();
        let result = reconcile_staged(&game, &empty, &stage).unwrap();
        assert_eq!(result.removed_files, 1);
        assert!(!mods.join("old.jar").exists());
        assert!(mods.join("unmanaged.jar").is_file());
        fs::remove_dir_all(game).unwrap();
    }

    #[test]
    fn rejects_collisions_and_bad_content_without_changing_existing_mods() {
        let game = temp_dir();
        let mods = game.join("mods");
        fs::create_dir_all(&mods).unwrap();
        fs::write(mods.join("user.jar"), b"user-owned").unwrap();
        let official = mod_jar("official");
        let collision_manifest = manifest("siege", &[("user.jar", &official)]);
        let staging = game.join("stage");
        fs::create_dir(&staging).unwrap();
        fs::write(staging.join("user.jar"), official).unwrap();
        assert!(reconcile_staged(&game, &collision_manifest, &staging).is_err());
        assert_eq!(fs::read(mods.join("user.jar")).unwrap(), b"user-owned");

        fs::remove_file(mods.join("user.jar")).unwrap();
        let expected = mod_jar("expected");
        let bad_manifest = manifest("siege", &[("bad.jar", &expected)]);
        let tampered = mod_jar("tampered");
        fs::write(staging.join("bad.jar"), tampered).unwrap();
        assert!(reconcile_staged(&game, &bad_manifest, &staging).is_err());
        assert!(!mods.join("bad.jar").exists());

        fs::create_dir_all(mods.join("personales")).unwrap();
        fs::write(mods.join("personales/custom.jar"), b"personal").unwrap();
        let custom = mod_jar("custom");
        let manifest = manifest("siege", &[("custom.jar", &custom)]);
        fs::write(staging.join("custom.jar"), custom).unwrap();
        assert!(reconcile_staged(&game, &manifest, &staging).is_err());
        assert_eq!(
            fs::read(mods.join("personales/custom.jar")).unwrap(),
            b"personal"
        );
        fs::remove_dir_all(&staging).unwrap();
        fs::remove_dir_all(game).unwrap();
    }

    #[test]
    fn rejects_non_mod_archives_even_when_manifest_hash_matches() {
        let game = temp_dir();
        let mut archive = ZipWriter::new(std::io::Cursor::new(Vec::new()));
        archive
            .start_file("readme.txt", SimpleFileOptions::default())
            .unwrap();
        archive.write_all(b"not a mod").unwrap();
        let bytes = archive.finish().unwrap().into_inner();
        let manifest = manifest("siege", &[("not-mod.jar", &bytes)]);
        let staging = game.join("stage");
        fs::create_dir(&staging).unwrap();
        fs::write(staging.join("not-mod.jar"), bytes).unwrap();
        assert!(reconcile_staged(&game, &manifest, &staging).is_err());
        assert!(!game.join("mods/not-mod.jar").exists());
        fs::remove_dir_all(&staging).unwrap();
        fs::remove_dir_all(game).unwrap();
    }
}
