use atomic_write_file::AtomicWriteFile;
use mc_launcher_core::net::http;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{mpsc, Mutex, MutexGuard},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const MANIFEST_LIMIT: u64 = 2 * 1024 * 1024;
const MOD_LIMIT: u64 = 512 * 1024 * 1024;
const PACK_LIMIT: u64 = 4 * 1024 * 1024 * 1024;
const STALE_STAGING_AGE: Duration = Duration::from_secs(3 * 24 * 60 * 60);
const MAX_PACK_DOWNLOAD_ATTEMPTS: usize = 3;
const PACK_DOWNLOAD_CONCURRENCY: usize = 4;
const ROLLBACK_FAILED_MARKER: &str = "ROLLBACK_FAILED";
static PACK_SYNC_LOCK: Mutex<()> = Mutex::new(());

pub(super) fn lock_pack_sync() -> Result<MutexGuard<'static, ()>, String> {
    PACK_SYNC_LOCK
        .lock()
        .map_err(|_| "El bloqueo de actualización del pack falló".to_string())
}

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
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
    pub message: String,
}

fn copy_download_with_progress<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    mut report_bytes: impl FnMut(u64),
) -> std::io::Result<u64> {
    let mut buffer = [0_u8; 64 * 1024];
    let mut copied = 0_u64;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        writer.write_all(&buffer[..read])?;
        copied += read as u64;
        report_bytes(copied);
    }
    Ok(copied)
}

fn valid_file_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 180
        && name.to_ascii_lowercase().ends_with(".jar")
        && name != "."
        && name != ".."
        && !name.starts_with('.')
        && !name.starts_with(' ')
        && !name.ends_with(' ')
        && !name.ends_with('.')
        && name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'.' | b'_' | b'-' | b'+' | b' ' | b'(' | b')' | b'[' | b']' | b'\'')
        })
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
    if !valid_pack_version(&manifest.version)
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
        if !is_allowed_download_url(&file.url, expected_series, &manifest.version, name) {
            return Err(format!("Origen de descarga no permitido para {name}"));
        }
    }
    if total > PACK_LIMIT {
        return Err("El tamaño total del pack supera 4 GiB".into());
    }
    Ok(())
}

pub(super) fn is_allowed_download_url(
    raw: &str,
    expected_series: &str,
    expected_version: &str,
    expected_name: &str,
) -> bool {
    let Ok(url) = reqwest::Url::parse(raw) else {
        return false;
    };
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return false;
    }
    url.host_str() == Some("github.com")
        && url.path()
            == format!(
                "/Santi-PdR/EternalCraft-Launcher/releases/download/{expected_series}-v{expected_version}/{}",
                encode_path_component(expected_name)
            )
}

fn encode_path_component(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

fn valid_pack_version(version: &str) -> bool {
    let parts: Vec<&str> = version.split('.').collect();
    parts.len() == 3
        && parts.iter().all(|part| {
            !part.is_empty()
                && part.len() <= 6
                && part.bytes().all(|byte| byte.is_ascii_digit())
                && (*part == "0" || !part.starts_with('0'))
        })
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
    load_previous_state_snapshot(path, series_id).map(|(state, _)| state)
}

fn load_previous_state_snapshot(
    path: &Path,
    series_id: &str,
) -> Result<(Option<PackState>, Option<Vec<u8>>), String> {
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
            Ok((Some(state), Some(bytes)))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok((None, None)),
        Err(error) => Err(format!("No se pudo leer el registro del pack: {error}")),
    }
}

fn verify_installed_mod_pair(
    official_file: &Path,
    loaded_file: &Path,
    expected_hash: &str,
    name: &str,
) -> Result<(), String> {
    check_normal_file_if_present(official_file)?;
    check_normal_file_if_present(loaded_file)?;
    if same_file::is_same_file(official_file, loaded_file)
        .map_err(|error| format!("No se pudo verificar el mod oficial {name}: {error}"))?
    {
        if digest_file(official_file)? != expected_hash {
            return Err(format!(
                "El mod oficial {name} cambió o está dañado. Actualiza el pack antes de jugar."
            ));
        }
        return Ok(());
    }
    if digest_file(official_file)? != expected_hash || digest_file(loaded_file)? != expected_hash {
        return Err(format!(
            "El mod oficial {name} cambió o está dañado. Actualiza el pack antes de jugar."
        ));
    }
    Ok(())
}

pub(super) fn verify_installed_pack(game_dir: &Path, series_id: &str) -> Result<(), String> {
    let _guard = lock_pack_sync()?;
    verify_installed_pack_locked(game_dir, series_id)
}

pub(super) fn verify_installed_pack_locked(
    game_dir: &Path,
    series_id: &str,
) -> Result<(), String> {
    let mods = game_dir.join("mods");
    let officials = mods.join("Oficiales");
    let metadata_dir = game_dir.join(".eternalcraft");
    let state_file = state_path(game_dir, series_id);
    check_normal_directory(&mods)?;
    check_normal_directory(&officials)?;
    check_normal_directory(&metadata_dir)?;
    check_normal_file_if_present(&state_file)?;
    let state = load_previous_state(&state_file, series_id)?
        .ok_or_else(|| "Primero instala o actualiza los archivos oficiales de esta serie.".to_string())?;
    if state.files.is_empty() {
        return Err("El manifiesto oficial no contiene mods verificables; no se iniciará una serie vacía.".into());
    }
    let expected: BTreeSet<&str> = state.files.keys().map(String::as_str).collect();
    let actual: BTreeSet<String> = match fs::read_dir(&officials) {
        Ok(entries) => entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                name.to_ascii_lowercase().ends_with(".jar").then_some(name)
            })
            .collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => BTreeSet::new(),
        Err(error) => return Err(format!("No se pudo revisar el pack oficial instalado: {error}")),
    };
    if actual.iter().map(String::as_str).collect::<BTreeSet<_>>() != expected {
        return Err("Los mods oficiales instalados no coinciden con el registro de la serie. Actualiza el pack antes de jugar.".into());
    }
    for (name, expected_hash) in &state.files {
        let official_file = officials.join(name);
        let loaded_file = mods.join(name);
        verify_installed_mod_pair(&official_file, &loaded_file, expected_hash, name)?;
    }
    Ok(())
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

fn is_launcher_staging_directory(name: &str) -> bool {
    let Some(suffix) = name.strip_prefix(".eternalcraft-pack-") else {
        return false;
    };
    let mut parts = suffix.split('-');
    matches!((parts.next(), parts.next(), parts.next()), (Some(pid), Some(nonce), None)
        if !pid.is_empty()
            && pid.bytes().all(|byte| byte.is_ascii_digit())
            && !nonce.is_empty()
            && nonce.bytes().all(|byte| byte.is_ascii_digit()))
}

fn is_stale_staging_directory(
    modified: SystemTime,
    now: SystemTime,
    max_age: Duration,
) -> bool {
    now.duration_since(modified)
        .is_ok_and(|age| age >= max_age)
}

fn clean_stale_staging_directories(
    game_dir: &Path,
    now: SystemTime,
    max_age: Duration,
) -> Result<usize, String> {
    let entries = match fs::read_dir(game_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(format!(
                "No se pudieron revisar descargas interrumpidas: {error}"
            ))
        }
    };
    let mut removed = 0;
    for entry in entries {
        let entry = entry.map_err(|error| {
            format!("No se pudo revisar una descarga interrumpida: {error}")
        })?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if !is_launcher_staging_directory(&name) {
            continue;
        }
        let path = entry.path();
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(format!(
                    "No se pudo validar una descarga interrumpida: {error}"
                ))
            }
        };
        if !metadata.file_type().is_dir() {
            continue;
        }
        match fs::symlink_metadata(path.join(ROLLBACK_FAILED_MARKER)) {
            Ok(_) => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "No se pudo validar si una descarga requiere recuperación: {error}"
                ))
            }
        }
        let Ok(modified) = metadata.modified() else {
            continue;
        };
        if !is_stale_staging_directory(modified, now, max_age) {
            continue;
        }
        match fs::remove_dir_all(&path) {
            Ok(()) => removed += 1,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(format!(
                    "No se pudo retirar una descarga interrumpida: {error}"
                ))
            }
        }
    }
    Ok(removed)
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
) -> Result<(), String> {
    for name in names {
        remove_normal_file_if_present(&mods_dir.join(name))?;
        remove_normal_file_if_present(&officials.join(name))?;
    }
    if let Some(state) = old_state {
        for name in state.files.keys() {
            let backup = staging.join("backup").join(name);
            check_normal_file_if_present(&backup)?;
            if backup.is_file() {
                fs::copy(&backup, officials.join(name)).map_err(|error| {
                    format!("No se pudo restaurar el respaldo oficial de {name}: {error}")
                })?;
                fs::hard_link(officials.join(name), mods_dir.join(name))
                    .or_else(|_| fs::copy(officials.join(name), mods_dir.join(name)).map(|_| ()))
                    .map_err(|error| {
                        format!("No se pudo restaurar el mod activo {name}: {error}")
                    })?;
            }
        }
    }
    match old_state_bytes {
        Some(bytes) => {
            let mut file = AtomicWriteFile::options()
                .open(state_path)
                .map_err(|error| format!("No se pudo restaurar el registro del pack: {error}"))?;
            file.write_all(bytes)
                .map_err(|error| format!("No se pudo restaurar el registro del pack: {error}"))?;
            file.commit()
                .map_err(|error| format!("No se pudo confirmar el registro restaurado: {error}"))?;
        }
        None => {
            remove_normal_file_if_present(state_path)?;
        }
    }
    Ok(())
}

fn mark_rollback_failed(staging: &Path, error: &str) -> Result<(), String> {
    let marker = staging.join(ROLLBACK_FAILED_MARKER);
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&marker)
        .map_err(|write_error| {
            format!(
                "No se pudo crear la marca de recuperación {}: {write_error}",
                marker.display()
            )
        })?;
    file.write_all(error.as_bytes())
        .map_err(|write_error| {
            format!("No se pudo guardar el detalle de recuperación: {write_error}")
        })?;
    file.sync_all().map_err(|write_error| {
        format!("No se pudo confirmar el detalle de recuperación: {write_error}")
    })
}

#[cfg(test)]
fn reconcile_staged(
    game_dir: &Path,
    manifest: &PackManifest,
    staging: &Path,
) -> Result<PackSyncResult, String> {
    let mut preserve_staging = false;
    reconcile_staged_with_recovery(game_dir, manifest, staging, &mut preserve_staging)
}

fn reconcile_staged_with_recovery(
    game_dir: &Path,
    manifest: &PackManifest,
    staging: &Path,
    preserve_staging: &mut bool,
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
    let (old_state, old_state_bytes) =
        load_previous_state_snapshot(&state_path, &manifest.series_id)?;
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
            if let Err(rollback_error) = restore_snapshot(
                &mods_dir,
                &officials,
                &state_path,
                &staging,
                &transaction_names,
                old_state.as_ref(),
                old_state_bytes.as_deref(),
            ) {
                *preserve_staging = true;
                let detail = format!(
                    "La actualización falló: {error}. La restauración también falló: {rollback_error}. Se conservaron los respaldos en {}",
                    staging.display()
                );
                if let Err(marker_error) = mark_rollback_failed(staging, &detail) {
                    return Err(format!("{detail}. {marker_error}"));
                }
                return Err(detail);
            }
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

fn retryable_pack_http_status(status: reqwest::StatusCode) -> bool {
    status == reqwest::StatusCode::REQUEST_TIMEOUT
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
}

fn retryable_pack_io_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::Interrupted
            | std::io::ErrorKind::TimedOut
            | std::io::ErrorKind::UnexpectedEof
    )
}

fn pack_download_retry_delay(attempt: usize, retry_after: Option<&str>) -> Duration {
    let seconds = retry_after
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(|seconds| seconds.clamp(1, 120))
        .unwrap_or_else(|| 1_u64 << attempt.saturating_sub(1).min(3));
    Duration::from_secs(seconds)
}

fn retry_pack_download_after(attempt: usize, retry_after: Option<&str>) {
    let delay = pack_download_retry_delay(attempt, retry_after);
    std::thread::sleep(delay);
}

fn fetch_pack_manifest(client: &reqwest::blocking::Client, url: &str) -> Result<Vec<u8>, String> {
    let mut last_error = String::new();
    for attempt in 1..=MAX_PACK_DOWNLOAD_ATTEMPTS {
        let response = match client.get(url).send() {
            Ok(response) if response.status().is_success() => response,
            Ok(response) => {
                let status = response.status();
                let retry_after = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                last_error = format!("El servidor devolvió {status} para el manifiesto oficial");
                if !retryable_pack_http_status(status) || attempt == MAX_PACK_DOWNLOAD_ATTEMPTS {
                    return Err(format!(
                        "No se pudo consultar el manifiesto oficial: {last_error}"
                    ));
                }
                retry_pack_download_after(attempt, retry_after.as_deref());
                continue;
            }
            Err(error) => {
                last_error = format!("No se pudo consultar el manifiesto oficial: {error}");
                if !(error.is_timeout() || error.is_connect())
                    || attempt == MAX_PACK_DOWNLOAD_ATTEMPTS
                {
                    return Err(last_error);
                }
                retry_pack_download_after(attempt, None);
                continue;
            }
        };
        let expected_length = response.content_length();
        if expected_length.is_some_and(|length| length > MANIFEST_LIMIT) {
            return Err("El manifiesto oficial supera el tamaño permitido".into());
        }
        let mut bytes = Vec::new();
        match response.take(MANIFEST_LIMIT + 1).read_to_end(&mut bytes) {
            Ok(_) if bytes.len() as u64 > MANIFEST_LIMIT => {
                return Err("El manifiesto oficial supera el tamaño permitido".into());
            }
            Ok(_) if expected_length.is_some_and(|length| length != bytes.len() as u64) => {
                last_error = "El manifiesto oficial llegó incompleto".into();
            }
            Ok(_) => return Ok(bytes),
            Err(error) => {
                last_error = format!("No se pudo leer el manifiesto oficial: {error}");
                if !retryable_pack_io_error(&error) || attempt == MAX_PACK_DOWNLOAD_ATTEMPTS {
                    return Err(last_error);
                }
            }
        }
        if attempt < MAX_PACK_DOWNLOAD_ATTEMPTS {
            retry_pack_download_after(attempt, None);
        }
    }
    Err(format!(
        "No se pudo consultar el manifiesto oficial después de {MAX_PACK_DOWNLOAD_ATTEMPTS} intentos: {last_error}"
    ))
}

fn download_pack_file(
    client: &reqwest::blocking::Client,
    file: &PackFile,
    staged: &Path,
    mut report_progress: impl FnMut(u64),
) -> Result<(), String> {
    let name = file.path.strip_prefix("mods/").unwrap();
    let mut last_error = String::new();
    for attempt in 1..=MAX_PACK_DOWNLOAD_ATTEMPTS {
        if staged.exists() {
            fs::remove_file(staged)
                .map_err(|error| {
                    format!("No se pudo retirar la descarga parcial de {name}: {error}")
                })?;
        }
        let response = match client.get(&file.url).send() {
            Ok(response) if response.status().is_success() => response,
            Ok(response) => {
                let status = response.status();
                let retry_after = response
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned);
                let error = response.error_for_status().unwrap_err();
                last_error = format!("No se pudo descargar {name} ({status}): {error}");
                if !retryable_pack_http_status(status) || attempt == MAX_PACK_DOWNLOAD_ATTEMPTS {
                    return Err(last_error);
                }
                retry_pack_download_after(attempt, retry_after.as_deref());
                continue;
            }
            Err(error) => {
                let retryable = error.is_timeout() || error.is_connect();
                last_error = format!("No se pudo descargar {name}: {error}");
                if !retryable || attempt == MAX_PACK_DOWNLOAD_ATTEMPTS {
                    return Err(last_error);
                }
                retry_pack_download_after(attempt, None);
                continue;
            }
        };
        if response
            .content_length()
            .is_some_and(|length| length != file.size_bytes)
        {
            last_error = format!("El servidor informó un tamaño inesperado para {name}");
            if attempt == MAX_PACK_DOWNLOAD_ATTEMPTS {
                return Err(last_error);
            }
            retry_pack_download_after(attempt, None);
            continue;
        }
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(staged)
            .map_err(|error| format!("No se pudo preparar la descarga de {name}: {error}"))?;
        let mut response = response.take(file.size_bytes + 1);
        let copied = match copy_download_with_progress(
            &mut response,
            &mut output,
            &mut report_progress,
        ) {
            Ok(copied) => copied,
            Err(error) => {
                drop(output);
                let _ = fs::remove_file(staged);
                last_error = format!("La descarga de {name} falló: {error}");
                if !retryable_pack_io_error(&error) || attempt == MAX_PACK_DOWNLOAD_ATTEMPTS {
                    return Err(last_error);
                }
                retry_pack_download_after(attempt, None);
                continue;
            }
        };
        if let Err(error) = output.sync_all() {
            drop(output);
            let _ = fs::remove_file(staged);
            return Err(format!("No se pudo confirmar la descarga de {name}: {error}"));
        }
        drop(output);
        if copied == file.size_bytes && digest_file(staged)? == file.sha256.to_ascii_lowercase() {
            return Ok(());
        }
        let _ = fs::remove_file(staged);
        last_error = format!("La descarga de {name} no pasó la verificación de tamaño y SHA-256");
        if attempt < MAX_PACK_DOWNLOAD_ATTEMPTS {
            retry_pack_download_after(attempt, None);
        }
    }
    Err(format!(
        "No se pudo completar {name} después de {MAX_PACK_DOWNLOAD_ATTEMPTS} intentos: {last_error}"
    ))
}

pub(super) enum PackDownloadEvent {
    Progress { index: usize, received_bytes: u64 },
    Finished { index: usize, result: Result<(), String> },
}

fn download_pack_files(
    client: &reqwest::blocking::Client,
    downloads: &[&PackFile],
    staging: &Path,
    series_id: &str,
    total_bytes: u64,
    mut report_progress: impl FnMut(PackProgress),
) -> Result<(), String> {
    let mut file_bytes = vec![0_u64; downloads.len()];
    let mut completed_files = 0;
    for batch_start in (0..downloads.len()).step_by(PACK_DOWNLOAD_CONCURRENCY) {
        let batch_end = (batch_start + PACK_DOWNLOAD_CONCURRENCY).min(downloads.len());
        let (sender, receiver) = mpsc::sync_channel(64);
        thread::scope(|scope| -> Result<(), String> {
            for index in batch_start..batch_end {
                let sender = sender.clone();
                let client = client.clone();
                let file = downloads[index];
                let staged = staging.join(file.path.strip_prefix("mods/").unwrap());
                scope.spawn(move || {
                    let result = download_pack_file(&client, file, &staged, |received_bytes| {
                        let _ = sender.send(PackDownloadEvent::Progress { index, received_bytes });
                    });
                    let _ = sender.send(PackDownloadEvent::Finished { index, result });
                });
            }
            drop(sender);
            let mut finished_in_batch = 0;
            let mut first_error = None;
            let mut last_report = None;
            while finished_in_batch < batch_end - batch_start {
                let event = receiver
                    .recv()
                    .map_err(|error| format!("Se interrumpió la recepción de descargas: {error}"))?;
                let (index, message, force_report) = match event {
                    PackDownloadEvent::Progress { index, received_bytes } => {
                        file_bytes[index] = received_bytes.min(downloads[index].size_bytes);
                        (index, None, false)
                    }
                    PackDownloadEvent::Finished { index, result } => {
                        finished_in_batch += 1;
                        match result {
                            Ok(()) => {
                                file_bytes[index] = downloads[index].size_bytes;
                                completed_files += 1;
                                (index, Some(format!("Verificado {}/{} · {}", index + 1, downloads.len(), downloads[index].path.strip_prefix("mods/").unwrap())), true)
                            }
                            Err(error) => {
                                first_error.get_or_insert(error);
                                (index, None, true)
                            }
                        }
                    }
                };
                let now = Instant::now();
                if force_report || super::progress_update_due(last_report, now, Duration::from_millis(200), false) {
                    if let Some(message) = message {
                        last_report = Some(now);
                        report_progress(PackProgress {
                            series_id: series_id.into(),
                            completed_files,
                            total_files: downloads.len(),
                            downloaded_bytes: file_bytes.iter().sum::<u64>().min(total_bytes),
                            total_bytes,
                            message,
                        });
                    } else if !force_report {
                        let name = downloads[index].path.strip_prefix("mods/").unwrap();
                        last_report = Some(now);
                        report_progress(PackProgress {
                            series_id: series_id.into(),
                            completed_files,
                            total_files: downloads.len(),
                            downloaded_bytes: file_bytes.iter().sum::<u64>().min(total_bytes),
                            total_bytes,
                            message: format!("Descargando {}/{} · {name}", index + 1, downloads.len()),
                        });
                    }
                }
            }
            if let Some(error) = first_error {
                Err(error)
            } else {
                Ok(())
            }
        })?;
    }
    Ok(())
}

pub(super) fn sync_pack_locked(
    game_dir: &Path,
    series_id: &str,
    minecraft_version: &str,
    loader: &str,
    loader_version: &str,
    mut report_progress: impl FnMut(PackProgress),
) -> Result<PackSyncResult, String> {
    let url = format!(
        "https://raw.githubusercontent.com/Santi-PdR/EternalCraft-Launcher/main/packs/{series_id}/manifest.json"
    );
    let client = http::client().map_err(|error| error.to_string())?;
    let manifest_bytes = fetch_pack_manifest(&client, &url)?;
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
    let total_bytes = downloads.iter().map(|file| file.size_bytes).sum::<u64>();
    let stale_directories = clean_stale_staging_directories(
        game_dir,
        SystemTime::now(),
        STALE_STAGING_AGE,
    )?;
    report_progress(PackProgress {
        series_id: series_id.into(),
        completed_files: 0,
        total_files: downloads.len(),
        downloaded_bytes: 0,
        total_bytes,
        message: format!(
            "Manifiesto {} verificado; preparando descargas{}",
            manifest.version,
            if stale_directories == 0 {
                String::new()
            } else {
                format!(" · limpiadas {stale_directories} descargas interrumpidas")
            }
        ),
    });

    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let staging = game_dir.join(format!(".eternalcraft-pack-{}-{nonce}", std::process::id()));
    fs::create_dir_all(&staging)
        .map_err(|error| format!("No se pudo preparar la actualización: {error}"))?;
    let mut preserve_staging = false;
    let result = (|| {
        let download_refs = downloads.iter().collect::<Vec<_>>();
        download_pack_files(
            &client,
            &download_refs,
            &staging,
            series_id,
            total_bytes,
            &mut report_progress,
        )?;
        report_progress(PackProgress {
            series_id: series_id.into(),
            completed_files: downloads.len(),
            total_files: downloads.len(),
            downloaded_bytes: total_bytes,
            total_bytes,
            message: "Instalando mods y retirando los oficiales obsoletos".into(),
        });
        reconcile_staged_with_recovery(game_dir, &manifest, &staging, &mut preserve_staging)
    })();
    if !preserve_staging {
        let _ = fs::remove_dir_all(&staging);
    }
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
                    url: format!("https://github.com/Santi-PdR/EternalCraft-Launcher/releases/download/{series_id}-v1.0.0/{name}"),
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

    #[test]
    fn state_snapshot_keeps_the_exact_rollback_bytes_and_propagates_read_errors() {
        let root = temp_dir();
        let path = root.join("siege-state.json");
        let expected = PackState {
            schema_version: 1,
            series_id: "siege".into(),
            version: "1.0.0".into(),
            files: BTreeMap::new(),
        };
        let bytes = serde_json::to_vec(&expected).unwrap();
        fs::write(&path, &bytes).unwrap();

        let (loaded, snapshot) = load_previous_state_snapshot(&path, "siege").unwrap();
        assert_eq!(loaded, Some(expected));
        assert_eq!(snapshot.as_deref(), Some(bytes.as_slice()));

        let missing = load_previous_state_snapshot(&root.join("missing.json"), "siege").unwrap();
        assert_eq!(missing, (None, None));

        let unreadable_path = root.join("directory-instead-of-state.json");
        fs::create_dir(&unreadable_path).unwrap();
        assert!(load_previous_state_snapshot(&unreadable_path, "siege")
            .unwrap_err()
            .contains("No se pudo leer el registro del pack"));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn installed_pack_checks_one_hash_for_hard_links_and_both_for_copies() {
        let root = temp_dir();
        let official = root.join("official.jar");
        let loaded = root.join("loaded.jar");
        let bytes = b"verified mod bytes";
        let hash = digest(bytes);

        fs::write(&official, bytes).unwrap();
        fs::hard_link(&official, &loaded).unwrap();
        verify_installed_mod_pair(&official, &loaded, &hash, "mod.jar").unwrap();

        fs::remove_file(&loaded).unwrap();
        fs::write(&loaded, bytes).unwrap();
        verify_installed_mod_pair(&official, &loaded, &hash, "mod.jar").unwrap();
        fs::write(&loaded, b"modified mod bytes").unwrap();
        assert!(verify_installed_mod_pair(&official, &loaded, &hash, "mod.jar").is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn interrupted_pack_staging_cleanup_only_targets_old_launcher_directories() {
        let game = temp_dir();
        let now = SystemTime::now();
        let stale_name = ".eternalcraft-pack-1234-987654321";
        let fresh_name = ".eternalcraft-pack-1234-987654322";
        let unrelated_name = ".eternalcraft-pack-not-owned";

        assert!(is_launcher_staging_directory(stale_name));
        assert!(!is_launcher_staging_directory(unrelated_name));
        assert!(!is_stale_staging_directory(
            now - Duration::from_secs(60),
            now,
            STALE_STAGING_AGE
        ));
        assert!(is_stale_staging_directory(now - STALE_STAGING_AGE, now, STALE_STAGING_AGE));

        fs::create_dir(game.join(stale_name)).unwrap();
        fs::create_dir(game.join(fresh_name)).unwrap();
        fs::create_dir(game.join(unrelated_name)).unwrap();
        let recovery_name = ".eternalcraft-pack-1234-987654323";
        fs::create_dir(game.join(recovery_name)).unwrap();
        fs::write(
            game.join(recovery_name).join(ROLLBACK_FAILED_MARKER),
            "keep backup",
        )
        .unwrap();
        let cleanup_time = now + Duration::from_secs(5);
        let removed = clean_stale_staging_directories(&game, cleanup_time, Duration::ZERO).unwrap();
        assert_eq!(
            removed, 2,
            "stale downloads are removed, but rollback recovery data is retained"
        );
        assert!(!game.join(stale_name).exists());
        assert!(!game.join(fresh_name).exists());
        assert!(game.join(unrelated_name).exists());
        assert!(game.join(recovery_name).join(ROLLBACK_FAILED_MARKER).is_file());
        fs::remove_dir_all(game).unwrap();
    }

    #[test]
    fn failed_snapshot_restore_reports_error_and_retains_backup_marker() {
        let game = temp_dir();
        let mods = game.join("mods");
        let officials = mods.join("Oficiales");
        let metadata = game.join(".eternalcraft");
        let staging = game.join(".eternalcraft-pack-1234-987654324");
        let backup = staging.join("backup");
        fs::create_dir_all(&mods).unwrap();
        fs::create_dir_all(&officials).unwrap();
        fs::create_dir_all(&metadata).unwrap();
        fs::create_dir_all(&backup).unwrap();
        fs::create_dir(mods.join("tracked.jar")).unwrap();
        fs::write(backup.join("tracked.jar"), b"recovery copy").unwrap();
        let previous_state = PackState {
            schema_version: 1,
            series_id: "siege".into(),
            version: "1.0.0".into(),
            files: BTreeMap::from([("tracked.jar".into(), "a".repeat(64))]),
        };
        let state_bytes = serde_json::to_vec(&previous_state).unwrap();

        let error = restore_snapshot(
            &mods,
            &officials,
            &state_path(&game, "siege"),
            &staging,
            &BTreeSet::from(["tracked.jar".into()]),
            Some(&previous_state),
            Some(&state_bytes),
        )
        .unwrap_err();
        assert!(error.contains("no es un archivo normal"));
        assert!(backup.join("tracked.jar").is_file());
        mark_rollback_failed(&staging, &error).unwrap();
        assert!(staging.join(ROLLBACK_FAILED_MARKER).is_file());
        assert_eq!(
            clean_stale_staging_directories(&game, SystemTime::now(), Duration::ZERO).unwrap(),
            0
        );
        assert!(backup.join("tracked.jar").is_file());
        fs::remove_dir_all(game).unwrap();
    }

    fn mod_jar(mod_id: &str) -> Vec<u8> {
        let mut archive = ZipWriter::new(std::io::Cursor::new(Vec::new()));
        archive
            .start_file("META-INF/mods.toml", SimpleFileOptions::default())
            .unwrap();
        write!(
            archive,
            "modLoader=\"javafml\"\nloaderVersion=\"[47,)\"\n[[mods]]\nmodId=\"{mod_id}\"\n"
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
        value.files[0].url = "https://github.com/Santi-PdR/EternalCraft-Launcher/releases/download/siege-v1.0.0/good.jar".into();
        validate_manifest(&value, "siege").unwrap();
        value.files[0].url = "https://github.com/Santi-PdR/EternalCraft-Launcher/releases/download/ghouls-outbreak-v1.0.0/good.jar".into();
        assert!(validate_manifest(&value, "siege").is_err());
        value.files[0].url = "https://github.com/Santi-PdR/EternalCraft-Launcher/releases/download/siege-v1.0.1/good.jar".into();
        assert!(validate_manifest(&value, "siege").is_err());
        value.files[0].url = "https://github.com/Santi-PdR/EternalCraft-Launcher/releases/download/siege-v1.0.0/other.jar".into();
        assert!(validate_manifest(&value, "siege").is_err());
        value.files[0].url = "https://github.com/Santi-PdR/EternalCraft-Launcher/releases/download/siege-v1.0.0/good.jar".into();
        value.files[0].url = "https://github.com/another-owner/another-repo/releases/download/siege-v1.0.0/mod.jar".into();
        assert!(validate_manifest(&value, "siege").is_err());
        value.files[0].url = "https://cdn.modrinth.com/data/AbCd1234/versions/EfGh5678/mod.jar".into();
        assert!(validate_manifest(&value, "siege").is_err());
        value.files[0].url = "https://raw.githubusercontent.com/Santi-PdR/EternalCraft-Launcher/main/mod.jar".into();
        assert!(validate_manifest(&value, "siege").is_err());
        assert!(validate_manifest(&value, "ghouls").is_err());

        let spaced_name = manifest("siege", &[("my mod.jar", valid)]);
        assert!(validate_manifest(&spaced_name, "siege").is_ok());

        value = manifest("siege", &[("good.jar", valid)]);
        value.version = "1.0.0/other".into();
        assert!(validate_manifest(&value, "siege").is_err());
    }

    #[test]
    fn streams_download_bytes_and_reports_incremental_progress() {
        let source = vec![42_u8; 128 * 1024 + 17];
        let mut reader = std::io::Cursor::new(source.as_slice());
        let mut output = Vec::new();
        let mut progress = Vec::new();

        let copied = copy_download_with_progress(&mut reader, &mut output, |received| {
            progress.push(received);
        })
        .unwrap();

        assert_eq!(copied, source.len() as u64);
        assert_eq!(output, source);
        assert_eq!(progress, vec![64 * 1024, 128 * 1024, 128 * 1024 + 17]);
    }

    #[test]
    fn pack_download_retries_only_transient_network_failures() {
        assert!(retryable_pack_http_status(reqwest::StatusCode::REQUEST_TIMEOUT));
        assert!(retryable_pack_http_status(reqwest::StatusCode::TOO_MANY_REQUESTS));
        assert!(retryable_pack_http_status(reqwest::StatusCode::BAD_GATEWAY));
        assert!(retryable_pack_http_status(reqwest::StatusCode::INTERNAL_SERVER_ERROR));
        assert!(!retryable_pack_http_status(reqwest::StatusCode::FORBIDDEN));
        assert!(!retryable_pack_http_status(reqwest::StatusCode::NOT_FOUND));

        assert!(retryable_pack_io_error(&std::io::Error::from(
            std::io::ErrorKind::ConnectionReset
        )));
        assert!(retryable_pack_io_error(&std::io::Error::from(
            std::io::ErrorKind::UnexpectedEof
        )));
        assert!(!retryable_pack_io_error(&std::io::Error::from(
            std::io::ErrorKind::PermissionDenied
        )));
    }

    #[test]
    fn pack_download_retry_delay_honors_bounded_retry_after() {
        assert_eq!(
            pack_download_retry_delay(1, Some("7")),
            Duration::from_secs(7)
        );
        assert_eq!(
            pack_download_retry_delay(1, Some("0")),
            Duration::from_secs(1)
        );
        assert_eq!(
            pack_download_retry_delay(1, Some("999")),
            Duration::from_secs(120)
        );
        assert_eq!(
            pack_download_retry_delay(3, Some("not-a-number")),
            Duration::from_secs(4)
        );
    }

    #[test]
    fn pack_manifest_fetch_retries_a_temporary_server_error() {
        use std::io::{BufRead, BufReader};
        use std::net::TcpListener;

        let body = b"{\"schemaVersion\":1}";
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                loop {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                }
                if attempt == 0 {
                    stream.write_all(
                        b"HTTP/1.1 503 Service Unavailable\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    ).unwrap();
                } else {
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .unwrap();
                    stream.write_all(body).unwrap();
                }
            }
        });
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .build()
            .unwrap();

        let fetched = fetch_pack_manifest(&client, &format!("http://{address}/manifest.json"))
            .unwrap();
        assert_eq!(fetched.as_slice(), body);
        server.join().unwrap();
    }

    #[test]
    fn pack_download_retries_a_temporary_server_error_and_verifies_the_file() {
        use std::io::{BufRead, BufReader};
        use std::net::TcpListener;

        let content = b"verified mod bytes";
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for attempt in 0..2 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                loop {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                }
                if attempt == 0 {
                    stream
                        .write_all(
                            b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        )
                        .unwrap();
                } else {
                    write!(
                        stream,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        content.len()
                    )
                    .unwrap();
                    stream.write_all(content).unwrap();
                }
            }
        });
        let root = temp_dir();
        let staged = root.join("mod.jar");
        let file = PackFile {
            path: "mods/mod.jar".into(),
            url: format!("http://{address}/mod.jar"),
            size_bytes: content.len() as u64,
            sha256: digest(content),
        };
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .build()
            .unwrap();
        let mut progress = Vec::new();

        download_pack_file(&client, &file, &staged, |bytes| progress.push(bytes)).unwrap();

        server.join().unwrap();
        assert_eq!(fs::read(&staged).unwrap(), content);
        assert_eq!(progress.last(), Some(&(content.len() as u64)));
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pack_files_download_in_parallel_with_a_bounded_worker_pool() {
        use std::io::{BufRead, BufReader};
        use std::net::TcpListener;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let file_count = PACK_DOWNLOAD_CONCURRENCY;
        let contents = (0..file_count)
            .map(|index| vec![index as u8 + 10; 128 * 1024])
            .collect::<Vec<_>>();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let active = Arc::new(AtomicUsize::new(0));
        let peak_active = Arc::new(AtomicUsize::new(0));
        let server_active = Arc::clone(&active);
        let server_peak = Arc::clone(&peak_active);
        let server_contents = contents.clone();
        let server = std::thread::spawn(move || {
            let workers = (0..file_count)
                .map(|_| {
                    let (mut stream, _) = listener.accept().unwrap();
                    let active = Arc::clone(&server_active);
                    let peak = Arc::clone(&server_peak);
                    let contents = server_contents.clone();
                    std::thread::spawn(move || {
                        let mut reader = BufReader::new(stream.try_clone().unwrap());
                        let mut request = String::new();
                        reader.read_line(&mut request).unwrap();
                        let file_index = request
                            .split_whitespace()
                            .nth(1)
                            .unwrap()
                            .trim_start_matches("/mods/mod-")
                            .trim_end_matches(".jar")
                            .parse::<usize>()
                            .unwrap();
                        let mut line = String::new();
                        loop {
                            line.clear();
                            reader.read_line(&mut line).unwrap();
                            if line == "\r\n" || line.is_empty() {
                                break;
                            }
                        }
                        let now_active = active.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now_active, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(150));
                        write!(
                            stream,
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            contents[file_index].len()
                        )
                        .unwrap();
                        stream.write_all(&contents[file_index]).unwrap();
                        active.fetch_sub(1, Ordering::SeqCst);
                    })
                })
                .collect::<Vec<_>>();
            for worker in workers {
                worker.join().unwrap();
            }
        });

        let files = contents
            .iter()
            .enumerate()
            .map(|(index, content)| PackFile {
                path: format!("mods/mod-{index}.jar"),
                url: format!("http://{address}/mods/mod-{index}.jar"),
                size_bytes: content.len() as u64,
                sha256: digest(content),
            })
            .collect::<Vec<_>>();
        let refs = files.iter().collect::<Vec<_>>();
        let total_bytes = files.iter().map(|file| file.size_bytes).sum();
        let staging = temp_dir();
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .build()
            .unwrap();
        let mut progress = Vec::new();

        download_pack_files(
            &client,
            &refs,
            &staging,
            "siege",
            total_bytes,
            |event| progress.push(event),
        )
        .unwrap();
        server.join().unwrap();

        assert!(peak_active.load(Ordering::SeqCst) > 1);
        for (index, content) in contents.iter().enumerate() {
            assert_eq!(fs::read(staging.join(format!("mod-{index}.jar"))).unwrap(), *content);
        }
        assert!(progress.iter().any(|event| event.completed_files == file_count));
        fs::remove_dir_all(staging).unwrap();
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
        fs::write(stage1.join("kept.jar"), &same).unwrap();
        reconcile_staged(&game, &first, &stage1).unwrap();
        fs::remove_dir_all(&stage1).unwrap();
        assert!(files_to_download(&game, &first).unwrap().is_empty());
        let updated = same.clone();
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

        let changed_kept = mod_jar("kept_v2");
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

    #[test]
    fn launch_validation_rejects_missing_stale_or_modified_official_mods() {
        let game = temp_dir();
        let mods = game.join("mods");
        let officials = mods.join("Oficiales");
        let metadata = game.join(".eternalcraft");
        fs::create_dir_all(&officials).unwrap();
        fs::create_dir_all(&metadata).unwrap();
        let bytes = b"verified mod payload";
        let hash = format!("{:x}", Sha256::digest(bytes));
        fs::write(mods.join("active.jar"), bytes).unwrap();
        fs::write(officials.join("active.jar"), bytes).unwrap();
        atomic_save_state(
            &state_path(&game, "siege"),
            &PackState {
                schema_version: 1,
                series_id: "siege".into(),
                version: "1.2.0".into(),
                files: BTreeMap::from([("active.jar".into(), hash)]),
            },
        )
        .unwrap();
        assert!(verify_installed_pack(&game, "siege").is_ok());
        fs::write(officials.join("retired.jar"), b"obsolete").unwrap();
        assert!(verify_installed_pack(&game, "siege").is_err());
        fs::remove_file(officials.join("retired.jar")).unwrap();
        fs::write(mods.join("active.jar"), b"modified by user").unwrap();
        assert!(verify_installed_pack(&game, "siege").is_err());
        fs::remove_dir_all(game).unwrap();
    }
}
