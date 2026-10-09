use atomic_write_file::AtomicWriteFile;
use mc_launcher_core::{
    account::Account,
    auth::microsoft_account::{complete_login, complete_refresh, get_secure_login_data, parse_auth_code_url},
    command::builder::LaunchOptions,
    launcher::Launcher,
    install::{
        client::{
            fetch_vanilla_version, install_version_files, load_version_json, write_version_json,
        },
        loader::{installer_command_args, InstallerInvocation},
    },
    loader::{
        forge::{forge_installed_version_id, installer_url},
        LoaderKind,
    },
    net::{
        download::{execute_plan, Checksum, DownloadPlan, DownloadTask},
        http,
    },
    progress::{ProgressEvent, ProgressReporter},
    runtime::{get_executable_path, get_installed_jvm_runtimes, install_jvm_runtime},
    types::CallbackDict,
};
use serde::{Deserialize, Serialize};
use sysinfo::System;
use std::sync::{OnceLock, RwLock};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    collections::BTreeMap,
    env, fs,
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::Command,
};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_dialog::DialogExt;

mod pack;
mod developer;

const CATALOG_JSON: &str = include_str!("../../resources/series/catalog.json");

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Series {
    id: String,
    name: String,
    subtitle: String,
    description: String,
    accent: String,
    minecraft_version: String,
    loader: String,
    loader_version: String,
    pack_status: PackStatus,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum PackStatus {
    Unpublished,
    Available,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Catalog {
    schema_version: u32,
    series: Vec<Series>,
}

static RUNTIME_CATALOG: OnceLock<RwLock<Option<Catalog>>> = OnceLock::new();
static RUNTIME_CATALOG_ONLINE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Settings {
    active_series_id: Option<String>,
    #[serde(default)]
    game_directories: BTreeMap<String, String>,
    #[serde(default)]
    java_executable: Option<String>,
    #[serde(default)]
    managed_installs: BTreeMap<String, String>,
    #[serde(default)]
    theme_id: Option<String>,
    #[serde(default)]
    background_file: Option<String>,
    #[serde(default)]
    microsoft_client_id: Option<String>,
    #[serde(default)]
    github_app_client_id: Option<String>,
    #[serde(default)]
    memory_limit_mb: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct MicrosoftProfile {
    username: String,
    uuid: String,
}

#[derive(Clone, Debug)]
struct AuthenticatedAccount {
    profile: MicrosoftProfile,
    access_token: String,
    refresh_token: String,
}

#[derive(Default)]
struct AuthSession(std::sync::Mutex<Option<AuthenticatedAccount>>);

#[derive(Default)]
struct MinecraftProcess(std::sync::Mutex<MinecraftProcessState>);

#[derive(Default)]
struct MinecraftProcessState {
    active: Option<RunningMinecraft>,
    last_status: MinecraftStatus,
}

struct RunningMinecraft {
    series_id: String,
    pid: u32,
    child: std::process::Child,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
struct MinecraftStatus {
    running: bool,
    series_id: Option<String>,
    pid: Option<u32>,
    exit_code: Option<i32>,
    exit_success: Option<bool>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Bootstrap {
    series: Vec<Series>,
    active_series_id: String,
    catalog_online: bool,
    game_directories: BTreeMap<String, String>,
    suggested_directories: BTreeMap<String, String>,
    config_directory: String,
    log_file: String,
    theme_id: String,
    background_path: Option<String>,
    java: JavaStatus,
    java_manually_selected: bool,
    managed_game_directories: BTreeMap<String, String>,
    installed_profiles: BTreeMap<String, String>,
    microsoft_client_id: Option<String>,
    github_app_client_id: Option<String>,
    developer_github_user: Option<String>,
    microsoft_profile: Option<MicrosoftProfile>,
    memory: MemoryStatus,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct MemoryStatus {
    total_mb: u32,
    min_mb: u32,
    max_mb: u32,
    selected_mb: u32,
    manually_selected: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InstallProgress {
    series_id: String,
    stage: String,
    message: String,
    completed_files: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct JavaStatus {
    executable: Option<String>,
    version: Option<String>,
    major: Option<u32>,
    compatible: bool,
    detail: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ModFileEntry {
    name: String,
    size_bytes: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct ModInventory {
    game_directory: Option<String>,
    mods_directory: Option<String>,
    loaded_from_mods_root: Vec<ModFileEntry>,
    official_store: Vec<ModFileEntry>,
    personal_store: Vec<ModFileEntry>,
}

const INSTALL_LOG_MAX_BYTES: u64 = 1_048_576;
const MAX_BACKGROUND_BYTES: u64 = 8 * 1024 * 1024;
static INSTALL_LOG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BackgroundFormat {
    Png,
    Jpeg,
    Webp,
    Gif,
}

impl BackgroundFormat {
    fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Webp => "webp",
            Self::Gif => "gif",
        }
    }
}

fn detect_background_format(header: &[u8]) -> Option<BackgroundFormat> {
    if header.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(BackgroundFormat::Png)
    } else if header.starts_with(b"\xff\xd8\xff") {
        Some(BackgroundFormat::Jpeg)
    } else if header.starts_with(b"GIF87a") || header.starts_with(b"GIF89a") {
        Some(BackgroundFormat::Gif)
    } else if header.len() >= 12 && &header[..4] == b"RIFF" && &header[8..12] == b"WEBP" {
        Some(BackgroundFormat::Webp)
    } else {
        None
    }
}

fn is_valid_theme(theme_id: &str) -> bool {
    matches!(theme_id, "series" | "siege" | "ghouls")
}

fn is_safe_background_filename(file_name: &str) -> bool {
    let Some(rest) = file_name.strip_prefix("background-") else {
        return false;
    };
    let Some((timestamp, extension)) = rest.split_once('.') else {
        return false;
    };
    !timestamp.is_empty()
        && timestamp.bytes().all(|byte| byte.is_ascii_digit())
        && matches!(extension, "png" | "jpg" | "webp" | "gif")
}

fn is_safe_staged_background_filename(file_name: &str) -> bool {
    let Some(rest) = file_name
        .strip_prefix(".background-")
        .and_then(|name| name.strip_suffix(".tmp"))
    else {
        return false;
    };
    !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_digit())
}

fn clean_staged_backgrounds(directory: &Path) -> Result<(), String> {
    match fs::symlink_metadata(directory) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => return Err("El almacenamiento de fondos no es un directorio normal".into()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "No se pudo validar el almacenamiento de fondos: {error}"
            ))
        }
    }
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!(
                "No se pudo revisar el almacenamiento de fondos: {error}"
            ))
        }
    };
    for entry in entries {
        let entry =
            entry.map_err(|error| format!("No se pudo revisar un fondo temporal: {error}"))?;
        let metadata = fs::symlink_metadata(entry.path())
            .map_err(|error| format!("No se pudo validar un fondo temporal: {error}"))?;
        let stale = metadata
            .modified()
            .ok()
            .and_then(|modified| std::time::SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age >= std::time::Duration::from_secs(60 * 60));
        if is_safe_staged_background_filename(&entry.file_name().to_string_lossy())
            && metadata.file_type().is_file()
            && stale
        {
            fs::remove_file(entry.path())
                .map_err(|error| format!("No se pudo limpiar un fondo interrumpido: {error}"))?;
        }
    }
    Ok(())
}

fn parse_catalog() -> Result<Catalog, String> {
    let catalog: Catalog = serde_json::from_str(CATALOG_JSON)
        .map_err(|error| format!("El catálogo integrado no es válido: {error}"))?;
    validate_catalog(catalog)
}

fn validate_catalog(catalog: Catalog) -> Result<Catalog, String> {
    if catalog.schema_version != 1 || catalog.series.is_empty() {
        return Err("La versión del catálogo no es compatible o no contiene series".into());
    }
    let mut ids = std::collections::BTreeSet::new();
    for series in &catalog.series {
        if series.id.is_empty()
            || series.id.len() > 64
            || !series.id.bytes().all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
            || !ids.insert(series.id.as_str())
        {
            return Err("El catálogo contiene identificadores de serie vacíos o duplicados".into());
        }
    }
    Ok(catalog)
}

fn current_catalog() -> Result<Catalog, String> {
    if let Some(catalog) = RUNTIME_CATALOG
        .get_or_init(|| RwLock::new(None))
        .read()
        .map_err(|_| "El catálogo local quedó bloqueado".to_string())?
        .clone()
    {
        return Ok(catalog);
    }
    parse_catalog()
}

fn refresh_runtime_catalog() -> Result<(), String> {
    if RUNTIME_CATALOG.get_or_init(|| RwLock::new(None)).read()
        .map_err(|_| "El catálogo local quedó bloqueado".to_string())?.is_some() {
        return Ok(());
    }
    const CATALOG_URL: &str = "https://raw.githubusercontent.com/Santi-PdR/EternalCraft-Launcher/main/resources/series/catalog.json";
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(8))
        .user_agent("EternalCraft-Launcher")
        .build()
        .map_err(|error| format!("No se pudo preparar la conexión del catálogo: {error}"))?;
    let response = client.get(CATALOG_URL).send()
        .map_err(|error| format!("No se pudo consultar el catálogo de EternalCraft: {error}"))?;
    if !response.status().is_success() {
        return Err(format!("GitHub respondió {} al consultar el catálogo", response.status()));
    }
    if response.content_length().is_some_and(|length| length > 1024 * 1024) {
        return Err("El catálogo remoto excede 1 MiB".into());
    }
    let bytes = response.bytes().map_err(|error| format!("No se pudo leer el catálogo remoto: {error}"))?;
    if bytes.len() > 1024 * 1024 {
        return Err("El catálogo remoto excede 1 MiB".into());
    }
    let catalog: Catalog = serde_json::from_slice(&bytes)
        .map_err(|error| format!("El catálogo remoto no es válido: {error}"))?;
    let catalog = validate_catalog(catalog)?;
    *RUNTIME_CATALOG.get_or_init(|| RwLock::new(None)).write()
        .map_err(|_| "El catálogo local quedó bloqueado".to_string())? = Some(catalog);
    RUNTIME_CATALOG_ONLINE.store(true, std::sync::atomic::Ordering::Relaxed);
    Ok(())
}

fn mark_runtime_series_available(series_id: &str) -> Result<(), String> {
    let mut catalog = current_catalog()?;
    let series = catalog.series.iter_mut().find(|series| series.id == series_id)
        .ok_or_else(|| format!("La serie {series_id} ya no existe en el catálogo"))?;
    series.pack_status = PackStatus::Available;
    *RUNTIME_CATALOG.get_or_init(|| RwLock::new(None)).write()
        .map_err(|_| "El catálogo local quedó bloqueado".to_string())? = Some(catalog);
    RUNTIME_CATALOG_ONLINE.store(true, std::sync::atomic::Ordering::Relaxed);
    Ok(())
}

fn settings_path(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = app
        .path()
        .app_config_dir()
        .map_err(|error| error.to_string())?;
    fs::create_dir_all(&dir)
        .map_err(|error| format!("No se pudo crear la configuración local: {error}"))?;
    Ok(dir.join("settings.json"))
}

fn read_settings(app: &AppHandle) -> Result<Settings, String> {
    let path = settings_path(app)?;
    match fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|error| format!("No se pudo leer {}: {error}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Settings::default()),
        Err(error) => Err(format!("No se pudo leer {}: {error}", path.display())),
    }
}

fn write_settings(app: &AppHandle, settings: &Settings) -> Result<(), String> {
    let path = settings_path(app)?;
    let bytes = serde_json::to_vec_pretty(settings).map_err(|error| error.to_string())?;
    let mut file = AtomicWriteFile::options()
        .open(&path)
        .map_err(|error| format!("No se pudo preparar la configuración: {error}"))?;
    file.write_all(&bytes)
        .map_err(|error| format!("No se pudo escribir la configuración: {error}"))?;
    file.commit()
        .map_err(|error| format!("No se pudo confirmar la configuración: {error}"))
}

fn valid_game_dir(path: &Path) -> Result<PathBuf, String> {
    if !path.is_dir() {
        return Err("La carpeta seleccionada ya no existe o no es un directorio".into());
    }
    path.canonicalize()
        .map_err(|error| format!("No se pudo resolver la carpeta seleccionada: {error}"))
}

fn managed_game_dir(app: &AppHandle, series_id: &str) -> Result<PathBuf, String> {
    let catalog = current_catalog()?;
    if !catalog.series.iter().any(|series| series.id == series_id) {
        return Err("La serie solicitada no existe en el catálogo".into());
    }
    let root = app
        .path()
        .app_data_dir()
        .map_err(|error| error.to_string())?;
    Ok(root.join("instances").join(series_id))
}

fn expected_profile_id(series: &Series) -> Option<String> {
    if series.loader.eq_ignore_ascii_case("forge") {
        forge_installed_version_id(&format!(
            "{}-{}",
            series.minecraft_version, series.loader_version
        ))
        .ok()
    } else {
        None
    }
}

fn parse_sha1_sidecar(content: &str) -> Result<String, String> {
    let value = content.split_whitespace().next().unwrap_or_default();
    if value.len() != 40 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("El repositorio de Forge devolvió un SHA-1 inválido para su instalador".into());
    }
    Ok(value.to_ascii_lowercase())
}

struct AppInstallProgress {
    app: AppHandle,
    series_id: String,
    stage: String,
    completed_files: u64,
    current_file: String,
}

impl AppInstallProgress {
    fn new(app: AppHandle, series_id: String) -> Self {
        Self {
            app,
            series_id,
            stage: "Preparando".into(),
            completed_files: 0,
            current_file: String::new(),
        }
    }

    fn set_stage(&mut self, stage: &str, message: &str) {
        self.stage = stage.into();
        self.emit(message);
    }

    fn emit(&self, message: &str) {
        let _ = append_install_log(&self.app, &self.series_id, &self.stage, message);
        let _ = self.app.emit(
            "forge-install-progress",
            InstallProgress {
                series_id: self.series_id.clone(),
                stage: self.stage.clone(),
                message: message.into(),
                completed_files: self.completed_files,
            },
        );
    }
}

fn append_install_log(
    app: &AppHandle,
    series_id: &str,
    stage: &str,
    message: &str,
) -> Result<(), String> {
    let app_data = app
        .path()
        .app_data_dir()
        .map_err(|error| format!("No se pudo ubicar el registro local: {error}"))?;
    let path = app_data.join("logs").join("launcher.log");
    append_install_log_at(&path, series_id, stage, message)
}

fn append_install_log_at(
    path: &Path,
    series_id: &str,
    stage: &str,
    message: &str,
) -> Result<(), String> {
    let _guard = INSTALL_LOG_LOCK
        .lock()
        .map_err(|_| "El bloqueo del registro local dejó de estar disponible".to_string())?;
    let parent = path
        .parent()
        .ok_or_else(|| "La ruta del registro no contiene una carpeta".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("No se pudo crear la carpeta del registro: {error}"))?;
    if fs::metadata(path).is_ok_and(|metadata| metadata.len() >= INSTALL_LOG_MAX_BYTES) {
        let previous = path.with_extension("log.1");
        let _ = fs::remove_file(&previous);
        fs::rename(path, previous)
            .map_err(|error| format!("No se pudo rotar el registro local: {error}"))?;
    }
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let safe_message = message.replace(['\n', '\r'], " ");
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| format!("No se pudo abrir el registro local: {error}"))?;
    writeln!(file, "{seconds} [{series_id}] [{stage}] {safe_message}")
        .map_err(|error| format!("No se pudo escribir el registro local: {error}"))
}

fn read_install_log_at(path: &Path) -> Result<String, String> {
    match fs::read_to_string(path) {
        Ok(content) => {
            const DISPLAY_LIMIT: usize = 256 * 1024;
            if content.len() <= DISPLAY_LIMIT {
                return Ok(content);
            }
            let minimum_start = content.len() - DISPLAY_LIMIT;
            let start = content
                .char_indices()
                .find(|(index, _)| *index >= minimum_start)
                .map(|(index, _)| index)
                .unwrap_or(content.len());
            let start = content[start..]
                .find('\n')
                .map(|offset| start + offset + 1)
                .unwrap_or(start);
            Ok(content[start..].to_string())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok("Todavía no hay registros de instalación.".into())
        }
        Err(error) => Err(format!("No se pudo leer el registro local: {error}")),
    }
}

fn read_log_tail(path: &Path, max_bytes: u64) -> Result<Option<String>, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => metadata,
        Ok(_) => return Err("El registro de Minecraft no es un archivo normal".into()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("No se pudo revisar el registro de Minecraft: {error}")),
    };
    let mut file = fs::File::open(path)
        .map_err(|error| format!("No se pudo abrir el registro de Minecraft: {error}"))?;
    let start = metadata.len().saturating_sub(max_bytes);
    let starts_at_line_boundary = if start == 0 {
        true
    } else {
        file.seek(SeekFrom::Start(start - 1))
            .map_err(|error| format!("No se pudo buscar el final del registro: {error}"))?;
        let mut previous_byte = [0_u8; 1];
        file.read_exact(&mut previous_byte)
            .map_err(|error| format!("No se pudo leer el registro de Minecraft: {error}"))?;
        previous_byte[0] == b'\n'
    };
    file.seek(SeekFrom::Start(start))
        .map_err(|error| format!("No se pudo buscar el final del registro: {error}"))?;
    let mut bytes = Vec::with_capacity(metadata.len().saturating_sub(start) as usize);
    file.take(max_bytes)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("No se pudo leer el registro de Minecraft: {error}"))?;
    let text = String::from_utf8_lossy(&bytes);
    let text = if start > 0 && !starts_at_line_boundary {
        text.find('\n')
            .map(|offset| &text[offset + 1..])
            .unwrap_or("")
    } else {
        text.as_ref()
    };
    Ok(Some(text.to_string()))
}

#[cfg(test)]
mod log_tests {
    use super::*;

    #[test]
    fn reads_only_complete_recent_lines_from_large_logs() {
        let path = std::env::temp_dir().join(format!(
            "eternalcraft-log-tail-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&path, b"123456\nabcdef\n").unwrap();
        assert_eq!(read_log_tail(&path, 7).unwrap().as_deref(), Some("abcdef\n"));
        assert_eq!(read_log_tail(&path, 8).unwrap().as_deref(), Some("abcdef\n"));
        fs::remove_file(path).unwrap();
    }
}

impl ProgressReporter for AppInstallProgress {
    fn report(&mut self, event: ProgressEvent) {
        match event {
            ProgressEvent::TaskStarted { label, .. } => self.current_file = label,
            ProgressEvent::TaskFinished { label } => {
                self.completed_files += 1;
                self.current_file = label;
                if self.completed_files == 1 || self.completed_files % 25 == 0 {
                    self.emit(&format!(
                        "{} archivos procesados · {}",
                        self.completed_files, self.current_file
                    ));
                }
            }
            ProgressEvent::TaskSkipped { label, .. } => {
                self.completed_files += 1;
                self.current_file = label;
                if self.completed_files == 1 || self.completed_files % 25 == 0 {
                    self.emit(&format!(
                        "{} archivos verificados · {}",
                        self.completed_files, self.current_file
                    ));
                }
            }
            ProgressEvent::BytesReceived {
                label,
                received,
                total,
            } => {
                self.current_file = label;
                let message = match total {
                    Some(total) => format!(
                        "Descargando {} · {} / {} bytes",
                        self.current_file, received, total
                    ),
                    None => format!("Descargando {} · {} bytes", self.current_file, received),
                };
                self.emit(&message);
            }
            ProgressEvent::StageStarted { stage } => {
                self.set_stage("Instalando", &format!("Etapa: {stage:?}"))
            }
        }
    }
}

fn install_forge_profile(
    app: AppHandle,
    series_id: String,
    minecraft_version: String,
    forge_version: String,
    java_executable: Option<PathBuf>,
    game_dir: PathBuf,
) -> Result<(), String> {
    fs::create_dir_all(&game_dir)
        .map_err(|error| format!("No se pudo preparar la instancia: {error}"))?;
    let mut progress = AppInstallProgress::new(app, series_id);
    progress.set_stage(
        "Minecraft",
        "Consultando los metadatos oficiales de Minecraft",
    );
    let vanilla = fetch_vanilla_version(&minecraft_version).map_err(|error| error.to_string())?;
    write_version_json(&game_dir, &vanilla).map_err(|error| error.to_string())?;
    let java_executable = if let Some(java) = java_executable {
        java
    } else {
        let runtime = vanilla
            .java_version
            .as_ref()
            .filter(|runtime| runtime.major_version == 17)
            .ok_or_else(|| {
                "Los metadatos oficiales no solicitaron el Java 17 esperado".to_string()
            })?;
        progress.set_stage(
            "Java",
            "Descargando y verificando el runtime Java 17 de Mojang",
        );
        install_jvm_runtime(&runtime.component, &game_dir, &CallbackDict::default())
            .map_err(|error| format!("No se pudo instalar el runtime oficial de Java: {error}"))?;
        let java = get_executable_path(&runtime.component, &game_dir)
            .ok_or_else(|| "Mojang descargó Java, pero no se encontró su ejecutable".to_string())?;
        ensure_java_executable(&java)?;
        let status = inspect_java(&java);
        if !status.compatible {
            return Err(format!(
                "El runtime descargado no es Java 17: {}",
                status.detail
            ));
        }
        java
    };
    progress.set_stage(
        "Minecraft",
        "Verificando archivos de Minecraft, bibliotecas y recursos",
    );
    install_version_files(&vanilla, &game_dir, &mut progress).map_err(|error| error.to_string())?;

    let full_forge_version = format!("{minecraft_version}-{forge_version}");
    let profile_id =
        forge_installed_version_id(&full_forge_version).map_err(|error| error.to_string())?;
    let profile_path = game_dir
        .join("versions")
        .join(&profile_id)
        .join(format!("{profile_id}.json"));
    let profile_is_valid =
        profile_path.is_file() && load_version_json(&game_dir, &profile_id).is_ok();
    if !profile_is_valid {
        progress.set_stage(
            "Forge",
            &format!("Descargando el instalador Forge {forge_version}"),
        );
        let installer_url = installer_url(&full_forge_version);
        let checksum =
            http::get_text(&format!("{installer_url}.sha1")).map_err(|error| error.to_string())?;
        let checksum = parse_sha1_sidecar(&checksum)?;
        let installer_path = game_dir
            .join("versions")
            .join(".installers")
            .join(format!("forge-{full_forge_version}-installer.jar"));
        let task = DownloadTask {
            url: installer_url,
            destination: installer_path.clone(),
            checksum: Some(Checksum::Sha1(checksum)),
            label: format!("Forge {forge_version} installer"),
        };
        execute_plan(&DownloadPlan { tasks: vec![task] }, &mut progress)
            .map_err(|error| error.to_string())?;

        progress.set_stage(
            "Forge",
            &format!("Aplicando Forge {forge_version} con Java 17"),
        );
        let invocation = InstallerInvocation {
            loader: LoaderKind::Forge,
            java_executable: java_executable.clone(),
            installer_path: installer_path.clone(),
            minecraft_dir: game_dir.clone(),
        };
        let output = Command::new(&invocation.java_executable)
            .args(installer_command_args(&invocation))
            .current_dir(&game_dir)
            .output()
            .map_err(|error| format!("No se pudo iniciar el instalador Forge: {error}"))?;
        let combined = format!(
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        for line in combined
            .lines()
            .rev()
            .take(20)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            progress.emit(line);
        }
        if !output.status.success() {
            return Err(format!("El instalador Forge terminó con código {:?}. Revisa los últimos mensajes del proceso.", output.status.code()));
        }
        let _ = fs::remove_file(installer_path);
    }

    progress.set_stage(
        "Verificación",
        "Comprobando el perfil Forge instalado y reparando archivos faltantes",
    );
    let forge = load_version_json(&game_dir, &profile_id)
        .map_err(|error| format!("El perfil Forge no quedó válido: {error}"))?;
    install_version_files(&forge, &game_dir, &mut progress).map_err(|error| error.to_string())?;
    progress.set_stage(
        "Terminado",
        "Base de Minecraft y Forge instalada y verificada",
    );
    Ok(())
}

fn list_jar_files(directory: &Path) -> Result<Vec<ModFileEntry>, String> {
    let mut files = Vec::new();
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(files),
        Err(error) => return Err(format!("No se pudo leer {}: {error}", directory.display())),
    };
    for entry in entries {
        let entry =
            entry.map_err(|error| format!("No se pudo leer una entrada de mods: {error}"))?;
        let path = entry.path();
        if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("jar"))
        {
            if let Ok(metadata) = fs::metadata(&path) {
                if metadata.is_file() {
                    files.push(ModFileEntry {
                        name: entry.file_name().to_string_lossy().into_owned(),
                        size_bytes: metadata.len(),
                    });
                }
            }
        }
    }
    files.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(files)
}

fn ensure_directory(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(()),
        Ok(_) => Err(format!(
            "No se modificó {} porque no es una carpeta normal",
            path.display()
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::create_dir(path)
            .map_err(|error| format!("No se pudo crear {}: {error}", path.display())),
        Err(error) => Err(format!(
            "No se pudo inspeccionar {}: {error}",
            path.display()
        )),
    }
}

fn validate_personal_mod_name(name: &str) -> Result<(), String> {
    let path = Path::new(name);
    let allowed = name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b" ._+-()[]'".contains(&byte));
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default();
    let reserved = stem.is_ascii()
        && (["CON", "PRN", "AUX", "NUL"]
            .into_iter()
            .any(|reserved| stem.eq_ignore_ascii_case(reserved))
            || (stem.len() == 4
                && (stem[..3].eq_ignore_ascii_case("COM")
                    || stem[..3].eq_ignore_ascii_case("LPT"))
                && stem.as_bytes()[3].is_ascii_digit()
                && stem.as_bytes()[3] != b'0'));
    if path.file_name().and_then(|part| part.to_str()) != Some(name)
        || name.is_empty()
        || !allowed
        || name.starts_with('.')
        || name.ends_with([' ', '.'])
        || !path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("jar"))
        || reserved
    {
        return Err("El nombre del mod debe ser un nombre de archivo JAR válido".into());
    }
    Ok(())
}

fn validate_forge_mod_archive(path: &Path) -> Result<(), String> {
    let file = fs::File::open(path)
        .map_err(|error| format!("No se pudo abrir el JAR {}: {error}", path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|error| format!("El archivo seleccionado no es un JAR válido: {error}"))?;
    if archive
        .file_names()
        .any(|name| name.eq_ignore_ascii_case("META-INF/mods.toml"))
    {
        return Ok(());
    }

    // Forge libraries (for example Kotlin for Forge) are valid JARs in the
    // mods directory even though they have no mods.toml. Only accept the
    // explicit marker that Forge itself uses; arbitrary JARs stay rejected.
    let manifest_index = archive
        .file_names()
        .position(|name| name.eq_ignore_ascii_case("META-INF/MANIFEST.MF"));
    let Some(manifest_index) = manifest_index else {
        return Err("El JAR no contiene mods.toml ni un manifiesto Forge de tipo LIBRARY".into());
    };
    let mut manifest = archive
        .by_index(manifest_index)
        .map_err(|error| format!("No se pudo leer el manifiesto del JAR: {error}"))?;
    let mut contents = String::new();
    manifest
        .by_ref()
        .take(64 * 1024)
        .read_to_string(&mut contents)
        .map_err(|error| format!("El manifiesto del JAR no es texto válido: {error}"))?;
    let is_forge_library = contents.lines().any(|line| {
        line.split_once(':').is_some_and(|(key, value)| {
            key.trim().eq_ignore_ascii_case("FMLModType")
                && value.trim().eq_ignore_ascii_case("LIBRARY")
        })
    });
    if !is_forge_library {
        return Err("El JAR no contiene mods.toml ni FMLModType: LIBRARY".into());
    }
    Ok(())
}

fn name_exists_case_insensitive(directory: &Path, name: &str) -> Result<bool, String> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(format!("No se pudo leer {}: {error}", directory.display())),
    };
    for entry in entries {
        let entry = entry.map_err(|error| format!("No se pudo revisar una entrada: {error}"))?;
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

fn copy_file_create_new(source: &Path, destination: &Path) -> io::Result<u64> {
    let mut input = fs::File::open(source)?;
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    let result = io::copy(&mut input, &mut output).and_then(|copied| {
        output.sync_all()?;
        Ok(copied)
    });
    if result.is_err() {
        drop(output);
        let _ = fs::remove_file(destination);
    }
    result
}

fn create_loaded_mod_alias(personal_file: &Path, loaded_file: &Path) -> Result<(), String> {
    match fs::hard_link(personal_file, loaded_file) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Err(format!(
            "Ya existe un archivo llamado {} en la carpeta activa de mods",
            loaded_file
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
        )),
        Err(_) => copy_file_create_new(personal_file, loaded_file)
            .map(|_| ())
            .map_err(|error| format!("No se pudo activar el mod en mods/: {error}")),
    }
}

fn add_personal_mod(source: &Path, mods_directory: &Path) -> Result<String, String> {
    let source = source
        .canonicalize()
        .map_err(|error| format!("No se pudo resolver el archivo seleccionado: {error}"))?;
    let metadata = fs::metadata(&source)
        .map_err(|error| format!("No se pudo leer el archivo seleccionado: {error}"))?;
    if !metadata.is_file() {
        return Err("Selecciona un archivo JAR normal".into());
    }
    let name = source
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| "El nombre del archivo no se puede usar en esta instancia".to_string())?;
    validate_personal_mod_name(name)?;
    validate_forge_mod_archive(&source)?;

    ensure_directory(mods_directory)?;
    let official_directory = mods_directory.join("Oficiales");
    let personal_directory = mods_directory.join("personales");
    ensure_directory(&official_directory)?;
    ensure_directory(&personal_directory)?;
    for directory in [
        mods_directory,
        official_directory.as_path(),
        personal_directory.as_path(),
    ] {
        if name_exists_case_insensitive(directory, name)? {
            return Err(format!(
                "Ya existe un mod con el nombre {name}; no se reemplazó ningún archivo"
            ));
        }
    }

    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary =
        personal_directory.join(format!(".{name}.{}.{}.tmp", std::process::id(), unique));
    if let Err(error) = copy_file_create_new(&source, &temporary) {
        let _ = fs::remove_file(&temporary);
        return Err(format!("No se pudo preparar la copia personal: {error}"));
    }
    if let Err(error) = validate_forge_mod_archive(&temporary) {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }

    let personal_file = personal_directory.join(name);
    match fs::hard_link(&temporary, &personal_file) {
        Ok(()) => {
            let _ = fs::remove_file(&temporary);
        }
        Err(_) => {
            if let Err(error) = copy_file_create_new(&temporary, &personal_file) {
                let _ = fs::remove_file(&temporary);
                return Err(format!("No se pudo guardar el mod personal: {error}"));
            }
            let _ = fs::remove_file(&temporary);
        }
    }

    let loaded_file = mods_directory.join(name);
    if let Err(error) = create_loaded_mod_alias(&personal_file, &loaded_file) {
        let _ = fs::remove_file(&personal_file);
        return Err(error);
    }
    Ok(name.to_string())
}

fn remove_personal_mod(name: &str, mods_directory: &Path) -> Result<(), String> {
    validate_personal_mod_name(name)?;
    let personal_directory = mods_directory.join("personales");
    let official_directory = mods_directory.join("Oficiales");
    ensure_directory(mods_directory)?;
    ensure_directory(&official_directory)?;
    ensure_directory(&personal_directory)?;
    if name_exists_case_insensitive(&official_directory, name)? {
        return Err(
            "Este archivo también figura en el almacenamiento oficial; no se eliminó".into(),
        );
    }
    let personal_file = personal_directory.join(name);
    if !personal_file.is_file() {
        return Err("No se encontró el mod personal seleccionado".into());
    }
    let loaded_file = mods_directory.join(name);
    match fs::symlink_metadata(&loaded_file) {
        Ok(metadata) if metadata.file_type().is_file() => {
            if !files_are_identical(&personal_file, &loaded_file)? {
                return Err(
                    "El archivo activo cambió y ya no coincide con el mod personal; no se eliminó".into(),
                );
            }
            fs::remove_file(&loaded_file)
                .map_err(|error| format!("No se pudo retirar el mod activo: {error}"))?;
        }
        Ok(metadata) if metadata.file_type().is_symlink() => {
            let personal_target = fs::canonicalize(&personal_file)
                .map_err(|error| format!("No se pudo resolver el mod personal: {error}"))?;
            let loaded_target = fs::canonicalize(&loaded_file)
                .map_err(|error| format!("No se pudo resolver el mod activo: {error}"))?;
            if loaded_target != personal_target {
                return Err(
                    "El enlace activo apunta a otro archivo; no se eliminó ningún mod".into(),
                );
            }
            fs::remove_file(&loaded_file)
                .map_err(|error| format!("No se pudo retirar el enlace activo: {error}"))?;
        }
        Ok(_) => return Err("La ruta activa del mod no es un archivo normal".into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("No se pudo revisar el mod activo: {error}")),
    }
    fs::remove_file(&personal_file)
        .map_err(|error| format!("No se pudo borrar el archivo personal: {error}"))
}

fn activate_personal_mod(name: &str, mods_directory: &Path) -> Result<(), String> {
    validate_personal_mod_name(name)?;
    let official_directory = mods_directory.join("Oficiales");
    let personal_directory = mods_directory.join("personales");
    ensure_directory(mods_directory)?;
    ensure_directory(&official_directory)?;
    ensure_directory(&personal_directory)?;
    if name_exists_case_insensitive(&official_directory, name)? {
        return Err("Este nombre pertenece a un mod oficial; no se activó como personal".into());
    }
    let personal_file = personal_directory.join(name);
    if !personal_file.is_file() {
        return Err("No se encontró el mod personal seleccionado".into());
    }
    validate_forge_mod_archive(&personal_file)?;
    let loaded_file = mods_directory.join(name);
    if name_exists_case_insensitive(mods_directory, name)? {
        if loaded_file.is_file() && files_are_identical(&personal_file, &loaded_file)? {
            return Ok(());
        }
        return Err(format!(
            "Ya existe un archivo distinto llamado {name} en mods/; no se reemplazó"
        ));
    }
    create_loaded_mod_alias(&personal_file, &loaded_file)
}

fn files_are_identical(left: &Path, right: &Path) -> Result<bool, String> {
    let left_size = fs::metadata(left)
        .map_err(|error| format!("No se pudo inspeccionar {}: {error}", left.display()))?
        .len();
    let right_size = fs::metadata(right)
        .map_err(|error| format!("No se pudo inspeccionar {}: {error}", right.display()))?
        .len();
    if left_size != right_size {
        return Ok(false);
    }
    let mut left = fs::File::open(left).map_err(|error| error.to_string())?;
    let mut right = fs::File::open(right).map_err(|error| error.to_string())?;
    let mut left_buffer = [0u8; 64 * 1024];
    let mut right_buffer = [0u8; 64 * 1024];
    loop {
        let left_read =
            std::io::Read::read(&mut left, &mut left_buffer).map_err(|error| error.to_string())?;
        let right_read = std::io::Read::read(&mut right, &mut right_buffer)
            .map_err(|error| error.to_string())?;
        if left_read != right_read || left_buffer[..left_read] != right_buffer[..right_read] {
            return Ok(false);
        }
        if left_read == 0 {
            return Ok(true);
        }
    }
}

fn suggested_directories() -> BTreeMap<String, String> {
    let mut result = BTreeMap::new();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        for (series_id, folder) in [("siege", "siege"), ("ghouls-outbreak", "ghouls")] {
            let candidate = home.join(".sklauncher").join("instances").join(folder);
            if candidate.is_dir() {
                if let Ok(path) = candidate.canonicalize() {
                    result.insert(series_id.to_string(), path.to_string_lossy().into_owned());
                }
            }
        }
    }
    result
}

fn parse_java_major(output: &str) -> Option<(String, u32)> {
    let marker = output.lines().find_map(|line| {
        let line = line.trim();
        if line.starts_with("openjdk version ")
            || line.starts_with("java version ")
            || line.starts_with("java ")
        {
            Some(line)
        } else {
            None
        }
    })?;
    let value = marker
        .split_once('"')
        .map(|(_, rest)| rest.split('"').next().unwrap_or(rest))
        .or_else(|| marker.strip_prefix("java "))?;
    let value = value.trim();
    let major = if let Some(rest) = value.strip_prefix("1.") {
        rest.split(|c: char| !c.is_ascii_digit())
            .next()?
            .parse()
            .ok()?
    } else {
        value
            .split(|c: char| !c.is_ascii_digit())
            .next()?
            .parse()
            .ok()?
    };
    (!value.is_empty()).then(|| (value.to_string(), major))
}

fn java_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(home) = env::var_os("JAVA_HOME").map(PathBuf::from) {
        candidates.push(
            home.join("bin")
                .join(if cfg!(windows) { "java.exe" } else { "java" }),
        );
    }
    if let Some(path) = env::var_os("PATH") {
        candidates.extend(
            env::split_paths(&path)
                .map(|dir| dir.join(if cfg!(windows) { "java.exe" } else { "java" })),
        );
    }
    #[cfg(target_os = "linux")]
    for root in ["/usr/lib/jvm", "/usr/java"] {
        if let Ok(entries) = fs::read_dir(root) {
            candidates.extend(entries.flatten().map(|entry| entry.path().join("bin/java")));
        }
    }
    candidates
}

fn detect_java() -> JavaStatus {
    let mut seen = std::collections::BTreeSet::new();
    let mut first_found = None;
    for candidate in java_candidates() {
        let normalized = candidate.to_string_lossy().into_owned();
        if !seen.insert(normalized.clone()) || !candidate.is_file() {
            continue;
        }
        match Command::new(&candidate).arg("-version").output() {
            Ok(output) if output.status.success() => {
                let text = format!(
                    "{}\n{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                if let Some((version, major)) = parse_java_major(&text) {
                    let compatible = major == 17;
                    let status = JavaStatus {
                        executable: Some(normalized),
                        version: Some(version),
                        major: Some(major),
                        compatible,
                        detail: if compatible {
                            "Java 17 detectado y compatible con Minecraft 1.20.1 / Forge".into()
                        } else {
                            format!("Java {major} detectado; Forge 1.20.1 requiere Java 17")
                        },
                    };
                    if compatible {
                        return status;
                    }
                    first_found.get_or_insert(status);
                }
            }
            _ => continue,
        }
    }
    first_found.unwrap_or_else(|| JavaStatus { executable: None, version: None, major: None, compatible: false,
        detail: "No se encontró una instalación de Java ejecutable. Se necesita Java 17 para Forge 1.20.1.".into() })
}

fn inspect_java(path: &Path) -> JavaStatus {
    let executable = path.to_string_lossy().into_owned();
    match Command::new(path).arg("-version").output() {
        Ok(output) => {
            let text = format!(
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            if let Some((version, major)) = parse_java_major(&text) {
                let compatible = major == 17;
                JavaStatus {
                    executable: Some(executable),
                    version: Some(version),
                    major: Some(major),
                    compatible,
                    detail: if compatible {
                        "Java 17 seleccionado y compatible con Minecraft 1.20.1 / Forge".into()
                    } else {
                        format!("Java {major} seleccionado; Forge 1.20.1 requiere Java 17")
                    },
                }
            } else {
                JavaStatus {
                    executable: Some(executable),
                    version: None,
                    major: None,
                    compatible: false,
                    detail: "El archivo seleccionado no devolvió una versión Java válida".into(),
                }
            }
        }
        Err(error) => JavaStatus {
            executable: Some(executable),
            version: None,
            major: None,
            compatible: false,
            detail: format!("No se pudo ejecutar Java: {error}"),
        },
    }
}

fn ensure_java_executable(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        let metadata = fs::metadata(path)
            .map_err(|error| format!("No se pudo inspeccionar el runtime Java: {error}"))?;
        let mode = metadata.permissions().mode();
        if mode & 0o111 == 0 {
            fs::set_permissions(path, fs::Permissions::from_mode(mode | 0o111))
                .map_err(|error| format!("No se pudo habilitar la ejecución de Java: {error}"))?;
        }
    }
    Ok(())
}

fn java_for_forge_install(
    status: &JavaStatus,
    manually_selected: bool,
) -> Result<Option<PathBuf>, String> {
    if status.compatible {
        return Ok(status.executable.as_deref().map(PathBuf::from));
    }
    if manually_selected {
        return Err(format!(
            "El Java seleccionado no es compatible. Selecciona Java 17 o vuelve a la detección automática. {}",
            status.detail
        ));
    }
    Ok(None)
}

fn memory_bounds(total_mb: u32) -> (u32, u32) {
    let min_mb = if total_mb < 1024 {
        (total_mb / 512 * 512).max(512)
    } else {
        1024
    };
    let max_mb = ((total_mb.saturating_mul(3) / 4) / 512 * 512)
        .min(12 * 1024)
        .max(min_mb);
    (min_mb, max_mb)
}

fn memory_status(settings: &Settings) -> MemoryStatus {
    static TOTAL_MEMORY_MB: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    let total_mb = *TOTAL_MEMORY_MB.get_or_init(|| {
        let mut system = System::new();
        system.refresh_memory();
        (system.total_memory() / (1024 * 1024)).min(u32::MAX as u64) as u32
    });
    let (min_mb, max_mb) = memory_bounds(total_mb);
    let default_mb = (6 * 1024).min(max_mb) / 512 * 512;
    let selected_mb = settings.memory_limit_mb.unwrap_or(default_mb).clamp(min_mb, max_mb);
    MemoryStatus {
        total_mb,
        min_mb,
        max_mb,
        selected_mb,
        manually_selected: settings.memory_limit_mb.is_some(),
    }
}

fn with_memory_arguments(
    args: &mut Vec<String>,
    main_class: &str,
    memory_mb: u32,
) -> Result<(), String> {
    let main_index = args
        .iter()
        .position(|argument| argument == main_class)
        .ok_or_else(|| {
            "No se encontró el límite entre argumentos JVM y argumentos de Minecraft".to_string()
        })?;
    let mut jvm_arguments = args
        .drain(..main_index)
        .filter(|argument| !argument.starts_with("-Xms") && !argument.starts_with("-Xmx"))
        .collect::<Vec<_>>();
    let initial_heap_mb = memory_mb.min(1024);
    jvm_arguments.push(format!("-Xms{initial_heap_mb}M"));
    jvm_arguments.push(format!("-Xmx{memory_mb}M"));
    jvm_arguments.append(args);
    *args = jvm_arguments;
    Ok(())
}

fn make_bootstrap(app: &AppHandle) -> Result<Bootstrap, String> {
    let catalog = current_catalog()?;
    let settings = read_settings(app)?;
    let memory = memory_status(&settings);
    let default_series = catalog
        .series
        .first()
        .expect("validated non-empty catalog")
        .id
        .clone();
    let active = settings
        .active_series_id
        .filter(|id| catalog.series.iter().any(|series| &series.id == id))
        .unwrap_or(default_series);
    let valid_ids: std::collections::BTreeSet<&str> = catalog
        .series
        .iter()
        .map(|series| series.id.as_str())
        .collect();
    let directories = settings
        .game_directories
        .into_iter()
        .filter(|(id, path)| valid_ids.contains(id.as_str()) && Path::new(path).is_dir())
        .collect();
    let config_directory = settings_path(app)?
        .parent()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let app_data_dir = app
        .path()
        .app_data_dir()
        .map_err(|error| error.to_string())?;
    let log_file = app_data_dir
        .join("logs")
        .join("launcher.log")
        .to_string_lossy()
        .into_owned();
    let microsoft_profile = app
        .state::<AuthSession>()
        .0
        .lock()
        .map_err(|_| "La sesión Microsoft quedó bloqueada".to_string())?
        .as_ref()
        .map(|session| session.profile.clone());
    let java_manually_selected = settings.java_executable.is_some();
    let java = if let Some(path) = settings.java_executable.as_deref() {
        inspect_java(Path::new(path))
    } else {
        let detected = detect_java();
        if detected.compatible {
            detected
        } else {
            let active_dir = app_data_dir.join("instances").join(&active);
            get_installed_jvm_runtimes(&active_dir)
                .into_iter()
                .filter_map(|component| get_executable_path(&component, &active_dir))
                .map(|path| inspect_java(&path))
                .find(|status| status.compatible)
                .unwrap_or(detected)
        }
    };
    let mut managed_game_directories = BTreeMap::new();
    let mut installed_profiles = BTreeMap::new();
    for series in &catalog.series {
        let game_dir = app_data_dir.join("instances").join(&series.id);
        managed_game_directories.insert(series.id.clone(), game_dir.to_string_lossy().into_owned());
        if let Some(profile_id) = expected_profile_id(series) {
            let profile_file = game_dir
                .join("versions")
                .join(&profile_id)
                .join(format!("{profile_id}.json"));
            if settings.managed_installs.get(&series.id) == Some(&profile_id)
                && profile_file.is_file()
                && load_version_json(&game_dir, &profile_id).is_ok()
            {
                installed_profiles.insert(series.id.clone(), profile_id);
            }
        }
    }
    Ok(Bootstrap {
        series: catalog.series,
        active_series_id: active,
        catalog_online: RUNTIME_CATALOG_ONLINE.load(std::sync::atomic::Ordering::Relaxed),
        game_directories: directories,
        suggested_directories: suggested_directories(),
        config_directory,
        log_file,
        theme_id: settings
            .theme_id
            .filter(|id| is_valid_theme(id))
            .unwrap_or_else(|| "series".into()),
        background_path: background_path(app, settings.background_file.as_deref())?,
        java,
        java_manually_selected,
        managed_game_directories,
        installed_profiles,
        microsoft_client_id: settings.microsoft_client_id,
        github_app_client_id: settings.github_app_client_id,
        developer_github_user: app.state::<developer::GitHubDeveloper>().username(),
        microsoft_profile,
        memory,
    })
}

fn background_path(app: &AppHandle, file_name: Option<&str>) -> Result<Option<String>, String> {
    let Some(file_name) = file_name.filter(|name| is_safe_background_filename(name)) else {
        return Ok(None);
    };
    let dir = app
        .path()
        .app_config_dir()
        .map_err(|error| error.to_string())?
        .join("appearance");
    clean_staged_backgrounds(&dir)?;
    let path = dir.join(file_name);
    match fs::symlink_metadata(&path) {
        Ok(metadata)
            if metadata.file_type().is_file() && metadata.len() <= MAX_BACKGROUND_BYTES =>
        {
            Ok(Some(path.to_string_lossy().into_owned()))
        }
        Ok(_) => Ok(None),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!("No se pudo validar el fondo: {error}")),
    }
}

#[tauri::command]
fn set_theme(app: AppHandle, theme_id: String) -> Result<Bootstrap, String> {
    if !is_valid_theme(&theme_id) {
        return Err("El tema solicitado no está disponible".into());
    }
    let mut settings = read_settings(&app)?;
    settings.theme_id = Some(theme_id);
    write_settings(&app, &settings)?;
    make_bootstrap(&app)
}

#[tauri::command]
fn select_background(app: AppHandle) -> Result<Bootstrap, String> {
    let selected = app
        .dialog()
        .file()
        .add_filter("Imágenes", &["png", "jpg", "jpeg", "webp", "gif"])
        .set_title("Elegir fondo del launcher (máximo 8 MB)")
        .blocking_pick_file();
    let Some(selected) = selected else {
        return make_bootstrap(&app);
    };
    let source = selected
        .into_path()
        .map_err(|error| format!("Ruta de imagen no válida: {error}"))?;
    let metadata = fs::symlink_metadata(&source)
        .map_err(|error| format!("No se pudo leer la imagen: {error}"))?;
    if !metadata.file_type().is_file()
        || metadata.len() == 0
        || metadata.len() > MAX_BACKGROUND_BYTES
    {
        return Err("El fondo debe ser un archivo normal de hasta 8 MB".into());
    }
    let mut input =
        fs::File::open(&source).map_err(|error| format!("No se pudo abrir la imagen: {error}"))?;
    let mut header = [0u8; 12];
    let read = input
        .read(&mut header)
        .map_err(|error| format!("No se pudo validar la imagen: {error}"))?;
    let format = detect_background_format(&header[..read])
        .ok_or("Formato no compatible. Usa PNG, JPEG, WebP o GIF.")?;
    let dir = app
        .path()
        .app_config_dir()
        .map_err(|error| error.to_string())?
        .join("appearance");
    ensure_directory(&dir)?;
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_nanos();
    clean_staged_backgrounds(&dir)?;
    let file_name = format!("background-{timestamp}.{}", format.extension());
    let staged = dir.join(format!(".background-{timestamp}.tmp"));
    let destination = dir.join(&file_name);
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staged)
        .map_err(|error| format!("No se pudo guardar el fondo: {error}"))?;
    let copied = match io::copy(&mut input.take(MAX_BACKGROUND_BYTES + 1), &mut output) {
        Ok(copied) if copied <= MAX_BACKGROUND_BYTES => copied,
        Ok(_) => {
            let _ = fs::remove_file(&staged);
            return Err("El fondo excede el límite de 8 MB".into());
        }
        Err(error) => {
            let _ = fs::remove_file(&staged);
            return Err(format!("No se pudo copiar el fondo: {error}"));
        }
    };
    if copied == 0 {
        let _ = fs::remove_file(&staged);
        return Err("La imagen está vacía".into());
    }
    if let Err(error) = output.sync_all() {
        let _ = fs::remove_file(&staged);
        return Err(format!("No se pudo confirmar el fondo en disco: {error}"));
    }
    drop(output);
    if let Err(error) = fs::rename(&staged, &destination) {
        let _ = fs::remove_file(&staged);
        return Err(format!("No se pudo confirmar el fondo: {error}"));
    }
    let mut settings = match read_settings(&app) {
        Ok(settings) => settings,
        Err(error) => {
            let _ = fs::remove_file(&destination);
            return Err(error);
        }
    };
    let previous = settings.background_file.replace(file_name);
    if let Err(error) = write_settings(&app, &settings) {
        let _ = fs::remove_file(&destination);
        return Err(error);
    }
    if let Some(old) = previous.filter(|name| is_safe_background_filename(name)) {
        if old != destination.file_name().unwrap().to_string_lossy() {
            let _ = fs::remove_file(dir.join(old));
        }
    }
    make_bootstrap(&app)
}

#[tauri::command]
fn clear_background(app: AppHandle) -> Result<Bootstrap, String> {
    let mut settings = read_settings(&app)?;
    let previous = settings.background_file.take();
    write_settings(&app, &settings)?;
    if let Some(name) = previous.filter(|name| is_safe_background_filename(name)) {
        if let Some(path) = app
            .path()
            .app_config_dir()
            .ok()
            .map(|dir| dir.join("appearance").join(name))
        {
            let _ = fs::remove_file(path);
        }
    }
    make_bootstrap(&app)
}

#[tauri::command]
async fn get_bootstrap(app: AppHandle) -> Result<Bootstrap, String> {
    let _ = tauri::async_runtime::spawn_blocking(move || refresh_runtime_catalog()).await;
    make_bootstrap(&app)
}

#[tauri::command]
fn get_launcher_logs(app: AppHandle, series_id: String) -> Result<String, String> {
    let path = app
        .path()
        .app_data_dir()
        .map_err(|error| error.to_string())?
        .join("logs")
        .join("launcher.log");
    let launcher_log = read_install_log_at(&path)?;
    let Some(game_directory) = resolve_series_game_directory(&app, &series_id)? else {
        return Ok(launcher_log);
    };
    let game_log_path = game_directory.join("logs").join("latest.log");
    let Some(game_log) = read_log_tail(&game_log_path, 256 * 1024)? else {
        return Ok(launcher_log);
    };
    Ok(format!(
        "=== EternalCraft Launcher ===\n{launcher_log}\n=== Minecraft · {} ===\n{}",
        series_id, game_log
    ))
}

#[tauri::command]
async fn sync_official_pack(
    app: AppHandle,
    series_id: String,
) -> Result<pack::PackSyncResult, String> {
    let series = current_catalog()?
        .series
        .into_iter()
        .find(|series| series.id == series_id)
        .ok_or_else(|| "La serie solicitada no existe en el catálogo".to_string())?;
    if series.pack_status != PackStatus::Available {
        return Err("Esta serie todavía no tiene un pack oficial publicado".into());
    }
    let game_dir = resolve_series_game_directory(&app, &series_id)?.ok_or_else(|| {
        "Primero instala o vincula una carpeta de juego para esta serie".to_string()
    })?;
    verify_forge_profile(&game_dir, &series)?;
    let progress_app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        pack::sync_pack(
            &game_dir,
            &series_id,
            &series.minecraft_version,
            &series.loader,
            &series.loader_version,
            move |progress| {
                let _ = progress_app.emit("pack-sync-progress", progress);
            },
        )
    })
    .await
    .map_err(|error| format!("Falló la tarea de sincronización del pack: {error}"))?
}

fn verify_forge_profile(game_dir: &Path, series: &Series) -> Result<(), String> {
    let profile_id = expected_profile_id(series)
        .ok_or_else(|| "La serie no define un perfil Forge compatible".to_string())?;
    let profile_path = game_dir
        .join("versions")
        .join(&profile_id)
        .join(format!("{profile_id}.json"));
    if !profile_path.is_file() {
        return Err(format!(
            "Forge {} no está instalado en esta instancia. Instala primero la base del juego.",
            series.loader_version
        ));
    }
    let profile = load_version_json(game_dir, &profile_id)
        .map_err(|error| format!("El perfil Forge de la instancia no es válido: {error}"))?;
    if profile.id.as_deref() != Some(profile_id.as_str())
        || profile.main_class.as_deref().is_none_or(str::is_empty)
    {
        return Err("El perfil Forge no contiene los datos de ejecución necesarios".into());
    }
    Ok(())
}

fn resolve_series_game_directory(
    app: &AppHandle,
    series_id: &str,
) -> Result<Option<PathBuf>, String> {
    let series = current_catalog()?
        .series
        .into_iter()
        .find(|series| series.id == series_id)
        .ok_or_else(|| "La serie solicitada no existe en el catálogo".to_string())?;
    let settings = read_settings(&app)?;
    if let Some(profile) = expected_profile_id(&series)
        .filter(|profile| settings.managed_installs.get(&series.id) == Some(profile))
    {
        let path = managed_game_dir(app, &series.id)?;
        let profile_file = path
            .join("versions")
            .join(&profile)
            .join(format!("{profile}.json"));
        if profile_file.is_file() && load_version_json(&path, &profile).is_ok() {
            return Ok(Some(path));
        }
    }
    if let Some(path) = settings.game_directories.get(series_id) {
        return valid_game_dir(Path::new(path)).map(Some);
    }
    Ok(None)
}

#[cfg(target_os = "windows")]
fn open_system_browser(url: &str) -> io::Result<()> {
    Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", url])
        .spawn()
        .map(|_| ())
}

#[cfg(target_os = "linux")]
fn open_system_browser(url: &str) -> io::Result<()> {
    Command::new("xdg-open")
        .arg(url)
        .spawn()
        .map(|_| ())
}

#[cfg(target_os = "macos")]
fn open_system_browser(url: &str) -> io::Result<()> {
    Command::new("open").arg(url).spawn().map(|_| ())
}

#[cfg(not(any(target_os = "windows", target_os = "linux", target_os = "macos")))]
fn open_system_browser(_url: &str) -> io::Result<()> {
    Err(io::Error::new(io::ErrorKind::Unsupported, "plataforma no compatible"))
}

fn is_valid_microsoft_client_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(index, byte)| {
            if [8, 13, 18, 23].contains(&index) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

#[tauri::command]
fn set_microsoft_client_id(app: AppHandle, client_id: String) -> Result<Bootstrap, String> {
    let id = client_id.trim();
    if !is_valid_microsoft_client_id(id) {
        return Err("El Client ID debe ser el identificador UUID de una aplicación de escritorio registrada en Microsoft.".into());
    }
    let mut settings = read_settings(&app)?;
    settings.microsoft_client_id = Some(id.to_ascii_lowercase());
    write_settings(&app, &settings)?;
    make_bootstrap(&app)
}

#[tauri::command]
fn login_microsoft(app: AppHandle) -> Result<Bootstrap, String> {
    let settings = read_settings(&app)?;
    let client_id = settings
        .microsoft_client_id
        .as_deref()
        .ok_or_else(|| "Configura primero el Client ID público de Microsoft en Ajustes.".to_string())?
        .to_string();
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
        .map_err(|error| format!("No se pudo abrir el callback local de inicio de sesión: {error}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("No se pudo preparar el callback local: {error}"))?;
    let port = listener.local_addr().map_err(|error| error.to_string())?.port();
    let redirect_uri = format!("http://localhost:{port}");
    let (login_url, expected_state, verifier) =
        get_secure_login_data(&client_id, &redirect_uri, None);
    open_system_browser(&login_url)
        .map_err(|error| format!("No se pudo abrir el navegador para iniciar sesión: {error}"))?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
    let auth_code = loop {
        if std::time::Instant::now() >= deadline {
            return Err("Se agotó el tiempo esperando el regreso del navegador. Intenta iniciar sesión otra vez.".into());
        }
        match listener.accept() {
            Ok((mut stream, peer)) => {
                if !peer.ip().is_loopback() {
                    continue;
                }
                let mut request = [0_u8; 8192];
                let read = stream
                    .read(&mut request)
                    .map_err(|error| format!("No se pudo leer el callback de Microsoft: {error}"))?;
                let request = String::from_utf8_lossy(&request[..read]);
                let first_line = request.lines().next().unwrap_or_default();
                let mut parts = first_line.split_ascii_whitespace();
                if parts.next() != Some("GET") {
                    let _ = stream.write_all(b"HTTP/1.1 405 Method Not Allowed\r\nConnection: close\r\n\r\n");
                    continue;
                }
                let Some(target) = parts.next().filter(|target| target.starts_with('/')) else {
                    let _ = stream.write_all(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n");
                    continue;
                };
                let callback = format!("{redirect_uri}{target}");
                let code = match parse_auth_code_url(&callback, Some(expected_state.clone())) {
                    Ok(code) => code,
                    Err(_) => {
                        let _ = stream.write_all(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\nNo se pudo validar el inicio de sesion.");
                        return Err("Microsoft devolvió un callback sin código válido o el estado de seguridad no coincide.".into());
                    }
                };
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\n\r\nInicio de sesion recibido. Puedes volver a EternalCraft Launcher.");
                break code;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(error) => return Err(format!("Falló el callback local de Microsoft: {error}")),
        }
    };

    let account = complete_login(&client_id, None, &redirect_uri, &auth_code, Some(&verifier));
    let account = account.map_err(|error| format!("No se pudo validar la cuenta de Minecraft: {error}"))?;
    let session = AuthenticatedAccount {
        profile: MicrosoftProfile {
            username: account.name,
            uuid: account.id,
        },
        access_token: account.access_token,
        refresh_token: account.refresh_token,
    };
    *app.state::<AuthSession>()
        .0
        .lock()
        .map_err(|_| "La sesión Microsoft quedó bloqueada".to_string())? = Some(session);
    make_bootstrap(&app)
}

#[tauri::command]
fn logout_microsoft(app: AppHandle) -> Result<Bootstrap, String> {
    *app.state::<AuthSession>()
        .0
        .lock()
        .map_err(|_| "La sesión Microsoft quedó bloqueada".to_string())? = None;
    make_bootstrap(&app)
}

#[tauri::command]
fn set_memory_limit(app: AppHandle, memory_mb: u32) -> Result<Bootstrap, String> {
    let mut settings = read_settings(&app)?;
    let limits = memory_status(&settings);
    if memory_mb < limits.min_mb || memory_mb > limits.max_mb || memory_mb % 512 != 0 {
        return Err(format!(
            "El valor debe estar entre {} y {} MiB en pasos de 512 MiB",
            limits.min_mb, limits.max_mb
        ));
    }
    settings.memory_limit_mb = Some(memory_mb);
    write_settings(&app, &settings)?;
    make_bootstrap(&app)
}

#[tauri::command]
fn reset_memory_limit(app: AppHandle) -> Result<Bootstrap, String> {
    let mut settings = read_settings(&app)?;
    settings.memory_limit_mb = None;
    write_settings(&app, &settings)?;
    make_bootstrap(&app)
}

fn minecraft_exit_status(series_id: &str, pid: u32, status: std::process::ExitStatus) -> MinecraftStatus {
    MinecraftStatus {
        running: false,
        series_id: Some(series_id.to_string()),
        pid: Some(pid),
        exit_code: status.code(),
        exit_success: Some(status.success()),
    }
}

#[tauri::command]
fn get_minecraft_status(app: AppHandle) -> Result<MinecraftStatus, String> {
    let process = app.state::<MinecraftProcess>();
    let mut state = process
        .0
        .lock()
        .map_err(|_| "El estado del proceso de Minecraft quedó bloqueado".to_string())?;
    let completed = match state.active.as_mut() {
        Some(active) => match active.child.try_wait() {
            Ok(Some(status)) => Some((active.series_id.clone(), active.pid, status)),
            Ok(None) => {
                return Ok(MinecraftStatus {
                    running: true,
                    series_id: Some(active.series_id.clone()),
                    pid: Some(active.pid),
                    exit_code: None,
                    exit_success: None,
                });
            }
            Err(error) => return Err(format!("No se pudo consultar Minecraft: {error}")),
        },
        None => return Ok(state.last_status.clone()),
    };
    let (series_id, pid, exit) = completed.ok_or_else(|| "No se pudo recuperar el estado de salida de Minecraft".to_string())?;
    let exit_success = exit.success();
    state.active = None;
    let status = minecraft_exit_status(&series_id, pid, exit);
    state.last_status = status.clone();
    drop(state);
    let message = match status.exit_code {
        Some(code) => format!("Minecraft finalizó con código {code} (éxito: {exit_success})"),
        None => "Minecraft finalizó sin código de salida (señal o terminación externa)".into(),
    };
    let _ = append_install_log(&app, &series_id, "Juego", &message);
    Ok(status)
}

#[tauri::command]
fn launch_minecraft(app: AppHandle, series_id: String) -> Result<MinecraftStatus, String> {
    let series = current_catalog()?
        .series
        .into_iter()
        .find(|series| series.id == series_id)
        .ok_or_else(|| "La serie solicitada no existe en el catálogo".to_string())?;
    if series.pack_status != PackStatus::Available {
        return Err("Esta serie todavía no tiene un pack oficial publicado y no se puede iniciar como serie jugable.".into());
    }
    let settings = read_settings(&app)?;
    let mut session = app
        .state::<AuthSession>()
        .0
        .lock()
        .map_err(|_| "La sesión Microsoft quedó bloqueada".to_string())?
        .clone()
        .ok_or_else(|| "Inicia sesión con una cuenta Microsoft propietaria de Minecraft antes de jugar.".to_string())?;
    let client_id = settings
        .microsoft_client_id
        .as_deref()
        .ok_or_else(|| "Falta el Client ID público de Microsoft.".to_string())?;
    let account = complete_refresh(client_id, None, &session.refresh_token)
        .map_err(|error| format!("La sesión Microsoft necesita renovarse. Vuelve a iniciar sesión: {error}"))?;
    session.profile = MicrosoftProfile {
        username: account.name.clone(),
        uuid: account.id.clone(),
    };
    session.access_token = account.access_token.clone();
    session.refresh_token = account.refresh_token.clone();
    *app.state::<AuthSession>()
        .0
        .lock()
        .map_err(|_| "La sesión Microsoft quedó bloqueada".to_string())? = Some(session);
    let game_dir = resolve_series_game_directory(&app, &series_id)?
        .ok_or_else(|| "Selecciona o instala la carpeta de juego de esta serie primero.".to_string())?;
    verify_forge_profile(&game_dir, &series)?;
    pack::verify_installed_pack(&game_dir, &series_id)?;
    let profile_id = expected_profile_id(&series).ok_or_else(|| "No se pudo determinar el perfil del juego".to_string())?;
    let mut java = if let Some(path) = settings.java_executable.as_deref() {
        inspect_java(Path::new(path))
    } else {
        detect_java()
    };
    if !java.compatible && settings.java_executable.is_none() {
        let app_data = app.path().app_data_dir().map_err(|error| error.to_string())?;
        let runtime_root = if settings.managed_installs.get(&series_id).map(String::as_str)
            == expected_profile_id(&series).as_deref()
        {
            app_data.join("instances").join(&series_id)
        } else {
            game_dir.clone()
        };
        java = get_installed_jvm_runtimes(&runtime_root)
            .into_iter()
            .filter_map(|component| get_executable_path(&component, &runtime_root))
            .map(|path| inspect_java(&path))
            .find(|status| status.compatible)
            .unwrap_or(java);
    }
    if !java.compatible {
        return Err(format!("Se necesita Java 17 para esta versión de Minecraft. {}", java.detail));
    }
    let launcher = Launcher::new(&game_dir);
    let version = launcher
        .load_version(&profile_id)
        .map_err(|error| format!("No se pudo cargar el perfil Forge instalado: {error}"))?;
    let mut command = launcher
        .build_launch_command_from_version(
            &version,
            LaunchOptions {
                account: Account::Microsoft {
                    username: account.name,
                    uuid: account.id,
                    access_token: account.access_token,
                },
                java_executable: Some(java.executable.ok_or_else(|| "No se encontró el ejecutable de Java 17".to_string())?.into()),
                game_directory: Some(game_dir.clone()),
                launcher_name: "EternalCraft".into(),
                launcher_version: env!("CARGO_PKG_VERSION").into(),
                ..Default::default()
            },
        )
        .map_err(|error| format!("No se pudieron preparar los argumentos de Minecraft: {error}"))?;
    let main_class = version
        .main_class
        .as_deref()
        .ok_or_else(|| "El perfil de Minecraft no declara su clase principal".to_string())?;
    with_memory_arguments(
        &mut command.args,
        main_class,
        memory_status(&settings).selected_mb,
    )?;
    let process = app.state::<MinecraftProcess>();
    let mut process_state = process
        .0
        .lock()
        .map_err(|_| "El estado del proceso de Minecraft quedó bloqueado".to_string())?;
    let previous_exit = match process_state.active.as_mut() {
        Some(active) => match active.child.try_wait() {
            Ok(None) => return Err("Minecraft ya está ejecutándose".into()),
            Ok(Some(exit)) => Some((active.series_id.clone(), active.pid, exit)),
            Err(error) => return Err(format!("No se pudo consultar el proceso anterior de Minecraft: {error}")),
        },
        None => None,
    };
    if let Some((previous_series, previous_pid, exit)) = previous_exit {
        let finished = minecraft_exit_status(&previous_series, previous_pid, exit);
        let message = format!("Minecraft finalizó; código: {:?}", finished.exit_code);
        process_state.active = None;
        process_state.last_status = finished;
        let _ = append_install_log(&app, &previous_series, "Juego", &message);
    }
    let child = Command::new(&command.executable)
        .args(&command.args)
        .current_dir(&command.working_dir)
        .spawn()
        .map_err(|error| {
            let message = format!("No se pudo iniciar Minecraft: {error}");
            let _ = append_install_log(&app, &series_id, "Error", &message);
            message
        })?;
    let pid = child.id();
    let status = MinecraftStatus {
        running: true,
        series_id: Some(series_id.clone()),
        pid: Some(pid),
        exit_code: None,
        exit_success: None,
    };
    process_state.active = Some(RunningMinecraft {
        series_id: series_id.clone(),
        pid,
        child,
    });
    process_state.last_status = status.clone();
    drop(process_state);
    let _ = append_install_log(&app, &series_id, "Juego", &format!("Minecraft iniciado con PID {pid}"));
    Ok(status)
}

fn mod_inventory_for_path(path: Option<PathBuf>) -> Result<ModInventory, String> {
    let Some(game_directory) = path else {
        return Ok(ModInventory {
            game_directory: None,
            mods_directory: None,
            loaded_from_mods_root: Vec::new(),
            official_store: Vec::new(),
            personal_store: Vec::new(),
        });
    };
    let mods_directory = game_directory.join("mods");
    Ok(ModInventory {
        game_directory: Some(game_directory.to_string_lossy().into_owned()),
        mods_directory: Some(mods_directory.to_string_lossy().into_owned()),
        loaded_from_mods_root: list_jar_files(&mods_directory)?,
        official_store: list_jar_files(&mods_directory.join("Oficiales"))?,
        personal_store: list_jar_files(&mods_directory.join("personales"))?,
    })
}

#[tauri::command]
fn get_mod_inventory(app: AppHandle, series_id: String) -> Result<ModInventory, String> {
    let game_directory = resolve_series_game_directory(&app, &series_id)?;
    mod_inventory_for_path(game_directory)
}

#[tauri::command]
fn add_personal_mod_to_series(app: AppHandle, series_id: String) -> Result<ModInventory, String> {
    let game_directory = resolve_series_game_directory(&app, &series_id)?
        .ok_or_else(|| "Primero selecciona una carpeta de juego para esta serie".to_string())?;
    let selected = app
        .dialog()
        .file()
        .add_filter("Mod de Forge", &["jar"])
        .set_title("Agregar mod personal de Forge")
        .blocking_pick_file();
    let Some(selected) = selected else {
        return get_mod_inventory(app, series_id);
    };
    let source = selected
        .into_path()
        .map_err(|error| format!("Ruta del mod no válida: {error}"))?;
    add_personal_mod(&source, &game_directory.join("mods"))?;
    get_mod_inventory(app, series_id)
}

#[tauri::command]
fn activate_personal_mod_for_series(
    app: AppHandle,
    series_id: String,
    file_name: String,
) -> Result<ModInventory, String> {
    let game_directory = resolve_series_game_directory(&app, &series_id)?
        .ok_or_else(|| "No hay una carpeta de juego seleccionada para esta serie".to_string())?;
    activate_personal_mod(&file_name, &game_directory.join("mods"))?;
    get_mod_inventory(app, series_id)
}

#[tauri::command]
fn remove_personal_mod_from_series(
    app: AppHandle,
    series_id: String,
    file_name: String,
) -> Result<ModInventory, String> {
    let game_directory = resolve_series_game_directory(&app, &series_id)?
        .ok_or_else(|| "No hay una carpeta de juego seleccionada para esta serie".to_string())?;
    remove_personal_mod(&file_name, &game_directory.join("mods"))?;
    get_mod_inventory(app, series_id)
}

#[tauri::command]
fn refresh_java_status(app: AppHandle) -> Result<Bootstrap, String> {
    make_bootstrap(&app)
}

#[tauri::command]
fn select_java_executable(app: AppHandle) -> Result<Bootstrap, String> {
    let selected = app
        .dialog()
        .file()
        .set_title("Seleccionar ejecutable Java 17")
        .blocking_pick_file();
    let Some(selected) = selected else {
        return make_bootstrap(&app);
    };
    let path = selected
        .into_path()
        .map_err(|error| format!("Ruta de Java no válida: {error}"))?;
    if !path.is_file() {
        return Err("El ejecutable Java seleccionado ya no existe".into());
    }
    let status = inspect_java(&path);
    if status.major != Some(17) {
        return Err(status.detail);
    }
    let mut settings = read_settings(&app)?;
    settings.java_executable = Some(path.to_string_lossy().into_owned());
    write_settings(&app, &settings)?;
    make_bootstrap(&app)
}

#[tauri::command]
fn reset_java_selection(app: AppHandle) -> Result<Bootstrap, String> {
    let mut settings = read_settings(&app)?;
    settings.java_executable = None;
    write_settings(&app, &settings)?;
    make_bootstrap(&app)
}

#[tauri::command]
async fn install_forge_base(app: AppHandle, series_id: String) -> Result<Bootstrap, String> {
    let series = current_catalog()?
        .series
        .into_iter()
        .find(|series| series.id == series_id)
        .ok_or_else(|| "La serie solicitada no existe en el catálogo".to_string())?;
    if !series.loader.eq_ignore_ascii_case("forge") {
        return Err("Esta serie todavía no tiene un instalador de cargador disponible".into());
    }
    let bootstrap = make_bootstrap(&app)?;
    let java = java_for_forge_install(&bootstrap.java, bootstrap.java_manually_selected)?;
    let game_dir = managed_game_dir(&app, &series.id)?;
    let managed_series_id = series.id.clone();
    let installed_profile_id = expected_profile_id(&series)
        .ok_or_else(|| "No se pudo identificar el perfil Forge instalado".to_string())?;
    let app_handle = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let log_app = app_handle.clone();
        let log_series = series.id.clone();
        let initial_message = "Preparando una instancia aislada del launcher";
        let _ = append_install_log(&log_app, &log_series, "Preparando", initial_message);
        let _ = app_handle.emit(
            "forge-install-progress",
            InstallProgress {
                series_id: series.id.clone(),
                stage: "Preparando".into(),
                message: initial_message.into(),
                completed_files: 0,
            },
        );
        let result = install_forge_profile(
            app_handle,
            series.id.clone(),
            series.minecraft_version,
            series.loader_version,
            java,
            game_dir,
        );
        if let Err(error) = &result {
            let _ = append_install_log(&log_app, &log_series, "Error", error);
            let _ = log_app.emit(
                "forge-install-progress",
                InstallProgress {
                    series_id: log_series,
                    stage: "Error".into(),
                    message: error.clone(),
                    completed_files: 0,
                },
            );
        }
        result
    })
    .await
    .map_err(|error| format!("Falló la tarea de instalación: {error}"))??;
    let mut settings = read_settings(&app)?;
    settings
        .managed_installs
        .insert(managed_series_id, installed_profile_id);
    write_settings(&app, &settings)?;
    make_bootstrap(&app)
}

#[tauri::command]
fn set_active_series(app: AppHandle, series_id: String) -> Result<Bootstrap, String> {
    let catalog = current_catalog()?;
    if !catalog.series.iter().any(|series| series.id == series_id) {
        return Err("La serie solicitada no existe en el catálogo".into());
    }
    let mut settings = read_settings(&app)?;
    settings.active_series_id = Some(series_id);
    write_settings(&app, &settings)?;
    make_bootstrap(&app)
}

#[tauri::command]
async fn select_game_directory(app: AppHandle, series_id: String) -> Result<Bootstrap, String> {
    let catalog = current_catalog()?;
    if !catalog.series.iter().any(|series| series.id == series_id) {
        return Err("La serie solicitada no existe en el catálogo".into());
    }
    let selected = app
        .dialog()
        .file()
        .set_title("Seleccionar carpeta de juego")
        .blocking_pick_folder();
    let Some(selected) = selected else {
        return make_bootstrap(&app);
    };
    let chosen = selected
        .into_path()
        .map_err(|error| format!("Ruta de carpeta no válida: {error}"))?;
    let chosen = valid_game_dir(&chosen)?;
    let mut settings = read_settings(&app)?;
    settings.active_series_id = Some(series_id.clone());
    settings
        .game_directories
        .insert(series_id, chosen.to_string_lossy().into_owned());
    write_settings(&app, &settings)?;
    make_bootstrap(&app)
}

#[tauri::command]
fn link_detected_directory(app: AppHandle, series_id: String) -> Result<Bootstrap, String> {
    let catalog = current_catalog()?;
    if !catalog.series.iter().any(|series| series.id == series_id) {
        return Err("La serie solicitada no existe en el catálogo".into());
    }
    let suggestions = suggested_directories();
    let path = suggestions
        .get(&series_id)
        .ok_or_else(|| "No se encontró una instancia compatible para esta serie".to_string())?;
    let path = valid_game_dir(Path::new(path))?;
    let mut settings = read_settings(&app)?;
    settings.active_series_id = Some(series_id.clone());
    settings
        .game_directories
        .insert(series_id, path.to_string_lossy().into_owned());
    write_settings(&app, &settings)?;
    make_bootstrap(&app)
}

pub fn run() {
    tauri::Builder::default()
        .manage(AuthSession::default())
        .manage(MinecraftProcess::default())
        .manage(developer::GitHubDeveloper::default())
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            get_bootstrap,
            set_theme,
            select_background,
            clear_background,
            get_launcher_logs,
            sync_official_pack,
            install_forge_base,
            get_mod_inventory,
            add_personal_mod_to_series,
            activate_personal_mod_for_series,
            remove_personal_mod_from_series,
            refresh_java_status,
            select_java_executable,
            reset_java_selection,
            set_active_series,
            select_game_directory,
            link_detected_directory,
            set_microsoft_client_id,
            login_microsoft,
            logout_microsoft,
            developer::set_github_app_client_id,
            developer::begin_github_developer_login,
            developer::poll_github_developer_login,
            developer::github_developer_status,
            developer::logout_github_developer,
            developer::choose_pack_source_directory,
            developer::refresh_pack_source_preview,
            developer::publish_pack_release,
            get_minecraft_status,
            set_memory_limit,
            reset_memory_limit,
            launch_minecraft
        ])
        .run(tauri::generate_context!())
        .expect("error while running EternalCraft Launcher");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_limits_reserve_system_memory_and_cap_large_allocations() {
        assert_eq!(memory_bounds(512), (512, 512));
        assert_eq!(memory_bounds(768), (512, 512));
        assert_eq!(memory_bounds(4096), (1024, 3072));
        assert_eq!(memory_bounds(16 * 1024), (1024, 12 * 1024));
    }

    #[cfg(unix)]
    #[test]
    fn minecraft_exit_status_records_success_and_failure_without_starting_game() {
        use std::os::unix::process::ExitStatusExt;

        let success = minecraft_exit_status("siege", 1234, std::process::ExitStatus::from_raw(0));
        assert!(!success.running);
        assert_eq!(success.series_id.as_deref(), Some("siege"));
        assert_eq!(success.pid, Some(1234));
        assert_eq!(success.exit_code, Some(0));
        assert_eq!(success.exit_success, Some(true));

        let failure = minecraft_exit_status(
            "ghouls-outbreak",
            5678,
            std::process::ExitStatus::from_raw(7 << 8),
        );
        assert!(!failure.running);
        assert_eq!(failure.series_id.as_deref(), Some("ghouls-outbreak"));
        assert_eq!(failure.pid, Some(5678));
        assert_eq!(failure.exit_code, Some(7));
        assert_eq!(failure.exit_success, Some(false));
    }

    #[cfg(windows)]
    #[test]
    fn minecraft_exit_status_records_success_and_failure_without_starting_game() {
        use std::os::windows::process::ExitStatusExt;

        let success = minecraft_exit_status("siege", 1234, std::process::ExitStatus::from_raw(0));
        assert!(!success.running);
        assert_eq!(success.series_id.as_deref(), Some("siege"));
        assert_eq!(success.pid, Some(1234));
        assert_eq!(success.exit_code, Some(0));
        assert_eq!(success.exit_success, Some(true));

        let failure = minecraft_exit_status(
            "ghouls-outbreak",
            5678,
            std::process::ExitStatus::from_raw(7),
        );
        assert!(!failure.running);
        assert_eq!(failure.series_id.as_deref(), Some("ghouls-outbreak"));
        assert_eq!(failure.pid, Some(5678));
        assert_eq!(failure.exit_code, Some(7));
        assert_eq!(failure.exit_success, Some(false));
    }

    #[test]
    fn memory_arguments_replace_only_jvm_heap_flags_before_main_class() {
        let mut args = vec![
            "-Xms512M".into(),
            "-cp".into(),
            "classpath".into(),
            "-Xmx2G".into(),
            "net.minecraft.client.main.Main".into(),
            "--username".into(),
            "-Xmxnot-a-jvm-flag".into(),
        ];
        with_memory_arguments(&mut args, "net.minecraft.client.main.Main", 4096).unwrap();
        assert_eq!(args.iter().filter(|arg| arg.starts_with("-Xms")).count(), 1);
        assert_eq!(args.iter().filter(|arg| arg.starts_with("-Xmx")).count(), 2);
        assert_eq!(args[0], "-cp");
        assert_eq!(args[2], "-Xms1024M");
        assert_eq!(args[3], "-Xmx4096M");
        assert_eq!(args[4], "net.minecraft.client.main.Main");
        assert_eq!(args[6], "-Xmxnot-a-jvm-flag");
    }

    #[test]
    fn memory_arguments_fail_without_a_main_class_boundary() {
        let mut args = vec!["-cp".into(), "libraries".into()];
        assert!(with_memory_arguments(&mut args, "missing.Main", 4096).is_err());
    }

    #[test]
    fn microsoft_client_id_requires_a_uuid() {
        assert!(is_valid_microsoft_client_id(
            "12345678-1234-4234-8234-123456789abc"
        ));
        assert!(!is_valid_microsoft_client_id("not-a-client-id"));
        assert!(!is_valid_microsoft_client_id(
            "12345678-1234-4234-8234-123456789ab"
        ));
    }

    #[test]
    fn microsoft_callback_requires_matching_state_and_an_auth_code() {
        let url = "http://localhost:49152/?code=one-time-code&state=expected";
        assert_eq!(
            parse_auth_code_url(url, Some("expected".to_string())).unwrap(),
            "one-time-code"
        );
        assert!(parse_auth_code_url(url, Some("attacker".to_string())).is_err());
        assert!(parse_auth_code_url(
            "http://localhost:49152/?error=access_denied&state=expected",
            Some("expected".to_string())
        )
        .is_err());
    }

    #[test]
    fn forge_archive_validation_accepts_library_jars_but_not_arbitrary_jars() {
        use std::io::Write as _;
        let fixture = std::env::temp_dir().join(format!(
            "eternalcraft-forge-library-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&fixture).unwrap();

        let library = fixture.join("kotlinforforge.jar");
        let mut archive = zip::ZipWriter::new(fs::File::create(&library).unwrap());
        archive
            .start_file(
                "META-INF/MANIFEST.MF",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        archive
            .write_all(b"Manifest-Version: 1.0\r\nFMLModType: LIBRARY\r\n\r\n")
            .unwrap();
        archive.finish().unwrap();
        validate_forge_mod_archive(&library).unwrap();

        let arbitrary = fixture.join("arbitrary.jar");
        let mut archive = zip::ZipWriter::new(fs::File::create(&arbitrary).unwrap());
        archive
            .start_file("META-INF/MANIFEST.MF", zip::write::SimpleFileOptions::default())
            .unwrap();
        archive.write_all(b"Manifest-Version: 1.0\r\n\r\n").unwrap();
        archive.finish().unwrap();
        assert!(validate_forge_mod_archive(&arbitrary).is_err());

        fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn bundled_catalog_is_valid_and_includes_the_initial_series() {
        let catalog = parse_catalog().expect("bundled catalog must parse");
        assert!(catalog.series.len() >= 2);
        assert!(catalog.series.iter().any(|series| series.id == "siege"));
        assert!(catalog
            .series
            .iter()
            .any(|series| series.id == "ghouls-outbreak"));
    }

    #[test]
    fn forge_profiles_use_the_expected_minecraft_profile_id() {
        let series = parse_catalog()
            .unwrap()
            .series
            .into_iter()
            .find(|series| series.id == "siege")
            .unwrap();
        assert_eq!(
            expected_profile_id(&series).as_deref(),
            Some("1.20.1-forge-47.4.10")
        );
        let game = std::env::temp_dir().join(format!(
            "eternalcraft-forge-profile-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        assert!(verify_forge_profile(&game, &series).is_err());
        fs::create_dir_all(game.join("versions/1.20.1-forge-47.4.10")).unwrap();
        fs::write(
            game.join("versions/1.20.1-forge-47.4.10/1.20.1-forge-47.4.10.json"),
            b"{}",
        )
        .unwrap();
        assert!(verify_forge_profile(&game, &series).is_err());
        fs::remove_dir_all(game).unwrap();
    }

    #[test]
    fn forge_installer_checksum_sidecar_is_strictly_validated() {
        let checksum = "01a5933597188fe57a0e18221108a457a55dcaef  forge-installer.jar\n";
        assert_eq!(
            parse_sha1_sidecar(checksum).unwrap(),
            "01a5933597188fe57a0e18221108a457a55dcaef"
        );
        assert!(parse_sha1_sidecar("not-a-checksum").is_err());
    }

    #[test]
    #[ignore = "descarga Minecraft/Forge/Java oficiales en un directorio temporal; nunca inicia Minecraft"]
    fn official_forge_install_smoke_test_without_launching_game() {
        struct QuietProgress;
        impl ProgressReporter for QuietProgress {
            fn report(&mut self, _: ProgressEvent) {}
        }

        let game_dir = std::env::temp_dir().join(format!(
            "eternalcraft-forge-smoke-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
        ));
        fs::create_dir_all(&game_dir).unwrap();
        let mut progress = QuietProgress;
        let vanilla = fetch_vanilla_version("1.20.1").unwrap();
        write_version_json(&game_dir, &vanilla).unwrap();
        let runtime = vanilla.java_version.as_ref().unwrap();
        assert_eq!(runtime.major_version, 17);
        install_jvm_runtime(&runtime.component, &game_dir, &CallbackDict::default()).unwrap();
        let java = get_executable_path(&runtime.component, &game_dir).unwrap();
        assert!(inspect_java(&java).compatible);
        install_version_files(&vanilla, &game_dir, &mut progress).unwrap();

        let forge_version = "1.20.1-47.4.10";
        let installer_url = installer_url(forge_version);
        let checksum =
            parse_sha1_sidecar(&http::get_text(&format!("{installer_url}.sha1")).unwrap()).unwrap();
        let installer = game_dir.join("forge-installer.jar");
        execute_plan(
            &DownloadPlan {
                tasks: vec![DownloadTask {
                    url: installer_url,
                    destination: installer.clone(),
                    checksum: Some(Checksum::Sha1(checksum)),
                    label: "Forge installer smoke test".into(),
                }],
            },
            &mut progress,
        )
        .unwrap();
        let invocation = InstallerInvocation {
            loader: LoaderKind::Forge,
            java_executable: java,
            installer_path: installer,
            minecraft_dir: game_dir.clone(),
        };
        let output = Command::new(&invocation.java_executable)
            .args(installer_command_args(&invocation))
            .current_dir(&game_dir)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "Forge installer failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let profile = load_version_json(&game_dir, "1.20.1-forge-47.4.10").unwrap();
        install_version_files(&profile, &game_dir, &mut progress).unwrap();
        assert!(game_dir
            .join("versions/1.20.1-forge-47.4.10/1.20.1-forge-47.4.10.json")
            .is_file());
        fs::remove_dir_all(game_dir).unwrap();
    }

    #[test]
    fn game_directory_requires_an_existing_directory() {
        assert!(valid_game_dir(Path::new("/path/that/does/not/exist/eternalcraft")).is_err());
        assert!(valid_game_dir(Path::new(env!("CARGO_MANIFEST_DIR"))).is_ok());
    }

    #[test]
    fn preferences_round_trip_with_multiple_series_directories() {
        let settings = Settings {
            active_series_id: Some("ghouls-outbreak".into()),
            game_directories: BTreeMap::from([
                ("siege".into(), "/games/siege".into()),
                ("ghouls-outbreak".into(), "/games/ghouls".into()),
            ]),
            java_executable: Some("/usr/lib/jvm/java-17/bin/java".into()),
            managed_installs: BTreeMap::from([("siege".into(), "1.20.1-forge-47.4.10".into())]),
            theme_id: Some("ghouls".into()),
            background_file: Some("background-123.webp".into()),
            microsoft_client_id: Some("12345678-1234-4234-8234-123456789abc".into()),
            github_app_client_id: Some("Iv23liAbCdEfGh123456".into()),
            memory_limit_mb: Some(4096),
        };
        let encoded = serde_json::to_vec(&settings).expect("settings serialize");
        let decoded: Settings = serde_json::from_slice(&encoded).expect("settings deserialize");
        assert_eq!(decoded.active_series_id.as_deref(), Some("ghouls-outbreak"));
        assert_eq!(decoded.game_directories, settings.game_directories);
        assert_eq!(decoded.java_executable, settings.java_executable);
        assert_eq!(decoded.managed_installs, settings.managed_installs);
        assert_eq!(decoded.theme_id.as_deref(), Some("ghouls"));
        assert_eq!(
            decoded.background_file.as_deref(),
            Some("background-123.webp")
        );
        assert_eq!(decoded.microsoft_client_id, settings.microsoft_client_id);
        assert_eq!(decoded.github_app_client_id, settings.github_app_client_id);
        assert_eq!(decoded.memory_limit_mb, Some(4096));
    }

    #[test]
    fn appearance_validation_accepts_only_supported_themes_and_local_image_signatures() {
        assert!(is_valid_theme("series"));
        assert!(is_valid_theme("siege"));
        assert!(is_valid_theme("ghouls"));
        assert!(!is_valid_theme("developer"));
        assert!(is_safe_background_filename("background-123.png"));
        assert!(!is_safe_background_filename("../background-123.png"));
        assert!(!is_safe_background_filename("background-x.png"));
        assert!(is_safe_staged_background_filename(".background-123.tmp"));
        assert!(!is_safe_staged_background_filename("../background-123.tmp"));
        assert!(!is_safe_staged_background_filename(".background-x.tmp"));
        assert_eq!(
            detect_background_format(b"\x89PNG\r\n\x1a\nrest"),
            Some(BackgroundFormat::Png)
        );
        assert_eq!(
            detect_background_format(b"RIFF1234WEBPrest"),
            Some(BackgroundFormat::Webp)
        );
        assert_eq!(detect_background_format(b"not an image"), None);
    }

    #[test]
    fn interrupted_background_cleanup_removes_only_owned_regular_temp_files() {
        let fixture = std::env::temp_dir().join(format!(
            "eternalcraft-background-cleanup-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&fixture).unwrap();
        let stale = fixture.join(".background-123.tmp");
        let unrelated = fixture.join("keep.tmp");
        fs::write(&stale, b"partial image").unwrap();
        fs::write(&unrelated, b"unrelated").unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(&stale)
            .unwrap()
            .set_times(fs::FileTimes::new().set_modified(
                std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 60 * 60),
            ))
            .unwrap();
        let active = fixture.join(".background-456.tmp");
        fs::write(&active, b"ongoing image").unwrap();

        clean_staged_backgrounds(&fixture).unwrap();
        assert!(!stale.exists());
        assert!(active.exists());
        assert_eq!(fs::read(&unrelated).unwrap(), b"unrelated");

        let not_a_directory = fixture.join("symlink-target");
        fs::write(&not_a_directory, b"keep").unwrap();
        assert!(clean_staged_backgrounds(&not_a_directory).is_err());
        assert_eq!(fs::read(&not_a_directory).unwrap(), b"keep");
        fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn parses_java_17_version_lines() {
        assert_eq!(
            parse_java_major("openjdk version \"17.0.12\" 2024-07-16\nOpenJDK Runtime Environment")
                .unwrap(),
            ("17.0.12".into(), 17)
        );
        assert_eq!(
            parse_java_major("java version \"1.8.0_412\"\nJava(TM) SE Runtime Environment")
                .unwrap(),
            ("1.8.0_412".into(), 8)
        );
        assert_eq!(
            parse_java_major("openjdk version \"21.0.3\" 2024-04-16").unwrap(),
            ("21.0.3".into(), 21)
        );
        assert!(parse_java_major("not a java version").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn managed_java_executable_gets_execute_permissions() {
        let fixture = std::env::temp_dir().join(format!(
            "eternalcraft-java-permissions-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&fixture, b"test executable").unwrap();
        fs::set_permissions(&fixture, fs::Permissions::from_mode(0o600)).unwrap();
        ensure_java_executable(&fixture).unwrap();
        assert_ne!(
            fs::metadata(&fixture).unwrap().permissions().mode() & 0o111,
            0
        );
        fs::remove_file(fixture).unwrap();
    }

    #[test]
    fn forge_install_uses_detected_java_or_provisions_when_automatic_java_is_incompatible() {
        let incompatible = JavaStatus {
            executable: Some("/usr/bin/java".into()),
            version: Some("25.0.4".into()),
            major: Some(25),
            compatible: false,
            detail: "Java 25 detectado; se requiere Java 17".into(),
        };
        assert!(java_for_forge_install(&incompatible, false)
            .unwrap()
            .is_none());
        assert!(java_for_forge_install(&incompatible, true).is_err());

        let compatible = JavaStatus {
            executable: Some("/usr/lib/jvm/java-17/bin/java".into()),
            version: Some("17.0.12".into()),
            major: Some(17),
            compatible: true,
            detail: "Java 17 compatible".into(),
        };
        assert_eq!(
            java_for_forge_install(&compatible, false).unwrap(),
            Some(PathBuf::from("/usr/lib/jvm/java-17/bin/java"))
        );
    }

    #[test]
    fn mod_inventory_scans_only_jar_files_immediately_in_each_store() {
        let fixture = std::env::temp_dir().join(format!(
            "eternalcraft-mod-inventory-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mods = fixture.join("mods");
        fs::create_dir_all(mods.join("Oficiales")).unwrap();
        fs::create_dir_all(mods.join("personales")).unwrap();
        fs::create_dir_all(mods.join("subfolder")).unwrap();
        fs::write(mods.join("loaded.jar"), b"loaded").unwrap();
        fs::write(mods.join("readme.txt"), b"ignored").unwrap();
        fs::write(mods.join("Oficiales").join("managed.jar"), b"official").unwrap();
        fs::write(mods.join("personales").join("custom.jar"), b"personal").unwrap();
        fs::write(mods.join("subfolder").join("nested.jar"), b"ignored").unwrap();

        let loaded = list_jar_files(&mods).unwrap();
        let official = list_jar_files(&mods.join("Oficiales")).unwrap();
        let personal = list_jar_files(&mods.join("personales")).unwrap();
        assert_eq!(
            loaded
                .iter()
                .map(|file| file.name.as_str())
                .collect::<Vec<_>>(),
            ["loaded.jar"]
        );
        assert_eq!(
            official
                .iter()
                .map(|file| file.name.as_str())
                .collect::<Vec<_>>(),
            ["managed.jar"]
        );
        assert_eq!(
            personal
                .iter()
                .map(|file| file.name.as_str())
                .collect::<Vec<_>>(),
            ["custom.jar"]
        );
        assert_eq!(loaded[0].size_bytes, 6);
        fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn personal_mod_is_stored_separately_and_loaded_from_the_forge_root() {
        use std::io::Write as _;
        let fixture = std::env::temp_dir().join(format!(
            "eternalcraft-personal-mod-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let source_dir = fixture.join("source");
        let game_dir = fixture.join("game");
        fs::create_dir_all(&source_dir).unwrap();
        fs::create_dir_all(&game_dir).unwrap();
        let source = source_dir.join("custom-mod.jar");
        let mut archive = zip::ZipWriter::new(fs::File::create(&source).unwrap());
        archive
            .start_file(
                "META-INF/mods.toml",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        archive
            .write_all(b"modLoader=\"javafml\"\nloaderVersion=\"[47,)\"\n")
            .unwrap();
        archive.finish().unwrap();

        let mods = game_dir.join("mods");
        assert_eq!(add_personal_mod(&source, &mods).unwrap(), "custom-mod.jar");
        assert!(mods.join("custom-mod.jar").is_file());
        assert!(mods.join("personales/custom-mod.jar").is_file());
        assert_eq!(list_jar_files(&mods).unwrap().len(), 1);
        assert_eq!(list_jar_files(&mods.join("personales")).unwrap().len(), 1);

        remove_personal_mod("custom-mod.jar", &mods).unwrap();
        assert!(!mods.join("custom-mod.jar").exists());
        assert!(!mods.join("personales/custom-mod.jar").exists());
        fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn personal_mod_can_be_activated_from_its_storage_folder() {
        use std::io::Write as _;
        let fixture = std::env::temp_dir().join(format!(
            "eternalcraft-personal-activate-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mods = fixture.join("mods");
        let personal_dir = mods.join("personales");
        fs::create_dir_all(&personal_dir).unwrap();
        let personal_file = personal_dir.join("stored.jar");
        let mut archive = zip::ZipWriter::new(fs::File::create(&personal_file).unwrap());
        archive
            .start_file(
                "META-INF/mods.toml",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        archive.write_all(b"modLoader=\"javafml\"").unwrap();
        archive.finish().unwrap();

        activate_personal_mod("stored.jar", &mods).unwrap();
        assert!(mods.join("stored.jar").is_file());
        activate_personal_mod("stored.jar", &mods).unwrap();
        assert!(files_are_identical(&personal_file, &mods.join("stored.jar")).unwrap());

        remove_personal_mod("stored.jar", &mods).unwrap();
        assert!(!personal_file.exists());
        fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn removing_personal_mod_preserves_replacement_file_in_mods_root() {
        let fixture = std::env::temp_dir().join(format!(
            "eternalcraft-personal-replacement-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mods = fixture.join("mods");
        let personal_dir = mods.join("personales");
        fs::create_dir_all(&personal_dir).unwrap();
        let personal_file = personal_dir.join("custom.jar");
        let loaded_file = mods.join("custom.jar");
        fs::write(&personal_file, b"original personal mod").unwrap();
        create_loaded_mod_alias(&personal_file, &loaded_file).unwrap();

        fs::remove_file(&loaded_file).unwrap();
        fs::write(&loaded_file, b"replacement owned by the user").unwrap();
        assert!(remove_personal_mod("custom.jar", &mods).is_err());
        assert_eq!(fs::read(&loaded_file).unwrap(), b"replacement owned by the user");
        assert_eq!(fs::read(&personal_file).unwrap(), b"original personal mod");
        fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn personal_mod_names_cannot_escape_the_mod_directories() {
        assert!(validate_personal_mod_name("..").is_err());
        assert!(validate_personal_mod_name("../outside.jar").is_err());
        assert!(validate_personal_mod_name("CON.jar").is_err());
        assert!(validate_personal_mod_name("éé.jar").is_err());
        assert!(validate_personal_mod_name("safe-mod.jar").is_ok());
    }

    #[test]
    fn personal_mod_import_rejects_invalid_archives_and_preserves_collisions() {
        use std::io::Write as _;
        let fixture = std::env::temp_dir().join(format!(
            "eternalcraft-personal-collision-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let source_dir = fixture.join("source");
        let mods = fixture.join("game/mods");
        fs::create_dir_all(&source_dir).unwrap();
        fs::create_dir_all(&mods).unwrap();
        let invalid = source_dir.join("invalid.jar");
        fs::write(&invalid, b"not a jar").unwrap();
        assert!(add_personal_mod(&invalid, &mods).is_err());
        assert!(!mods.join("personales").exists());

        let source = source_dir.join("taken.jar");
        let mut archive = zip::ZipWriter::new(fs::File::create(&source).unwrap());
        archive
            .start_file(
                "META-INF/mods.toml",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        archive.write_all(b"modLoader=\"javafml\"").unwrap();
        archive.finish().unwrap();
        let occupied = mods.join("taken.jar");
        fs::write(&occupied, b"keep this user's file").unwrap();
        assert!(add_personal_mod(&source, &mods).is_err());
        assert_eq!(fs::read(&occupied).unwrap(), b"keep this user's file");
        assert!(!mods.join("personales/taken.jar").exists());
        fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn install_log_rotates_and_sanitizes_multiline_messages() {
        let fixture = std::env::temp_dir().join(format!(
            "eternalcraft-log-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&fixture).unwrap();
        let log = fixture.join("launcher.log");
        fs::write(&log, vec![b'x'; INSTALL_LOG_MAX_BYTES as usize]).unwrap();
        append_install_log_at(&log, "siege", "Forge", "salida uno\nsalida dos\rfin").unwrap();
        assert!(fixture.join("launcher.log.1").is_file());
        let current = fs::read_to_string(&log).unwrap();
        assert!(current.contains("[siege] [Forge] salida uno salida dos fin"));
        assert!(!current.contains('\r'));
        assert!(!current.contains("\nsalida dos"));
        fs::remove_dir_all(fixture).unwrap();
    }
}
