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
    runtime::{get_executable_path, get_installed_jvm_runtimes},
};
use serde::{Deserialize, Serialize};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};
use std::ffi::{OsStr, OsString};
use std::sync::{OnceLock, RwLock};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    env, fs,
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_dialog::DialogExt;
use keyring::{Entry as CredentialEntry, Error as CredentialError};
use zeroize::{Zeroize, Zeroizing};

mod pack;
mod developer;
mod mojang_runtime;

const CATALOG_JSON: &str = include_str!("../../resources/series/catalog.json");
const MAX_FORGE_MOD_METADATA_BYTES: u64 = 256 * 1024;
const MAX_FORGE_MOD_ARCHIVE_ENTRIES: u16 = 50_000;
const MAX_FORGE_MOD_CENTRAL_DIRECTORY_BYTES: u32 = 32 * 1024 * 1024;
const FORGE_INSTALL_TIMEOUT: Duration = Duration::from_secs(20 * 60);
const FORGE_METADATA_ATTEMPTS: usize = 3;
const FORGE_METADATA_RETRY_DELAY: Duration = Duration::from_secs(2);

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
static RUNTIME_CATALOG_LAST_ATTEMPT: OnceLock<std::sync::Mutex<Option<Instant>>> = OnceLock::new();

const CATALOG_REFRESH_INTERVAL: Duration = Duration::from_secs(5 * 60);
const CATALOG_RETRY_INTERVAL: Duration = Duration::from_secs(30);
const MAX_CATALOG_BYTES: usize = 1024 * 1024;

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
    theme_migrated: bool,
    #[serde(default)]
    background_preset: Option<String>,
    #[serde(default, skip_serializing)]
    microsoft_client_id: Option<String>,
    #[serde(default)]
    memory_limit_mb: Option<u32>,
    #[serde(default)]
    offline_username: Option<String>,
    #[serde(default)]
    offline_uuid: Option<String>,
    #[serde(default)]
    account_mode: Option<AccountMode>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct MicrosoftProfile {
    username: String,
    uuid: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum AccountMode {
    Microsoft,
    Offline,
}

#[derive(Clone, Debug)]
struct AuthenticatedAccount {
    profile: MicrosoftProfile,
    access_token: String,
    refresh_token: String,
}

impl Drop for AuthenticatedAccount {
    fn drop(&mut self) {
        self.access_token.zeroize();
        self.refresh_token.zeroize();
    }
}

#[derive(Default)]
struct AuthSession(std::sync::Mutex<Option<AuthenticatedAccount>>);

#[derive(Default)]
struct MicrosoftSessionStatus(std::sync::Mutex<MicrosoftSessionStatusData>);

#[derive(Default)]
struct MicrosoftSessionStatusData {
    restore_started: bool,
    restore_attempted: bool,
    notice: Option<String>,
}

const MICROSOFT_CREDENTIAL_SERVICE: &str = "com.eternalcraft.launcher";
const MICROSOFT_CREDENTIAL_USER: &str = "minecraft-refresh-token";
static MICROSOFT_CREDENTIAL_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn microsoft_credential_entry() -> Result<CredentialEntry, String> {
    CredentialEntry::new(MICROSOFT_CREDENTIAL_SERVICE, MICROSOFT_CREDENTIAL_USER)
        .map_err(|_| "No se pudo abrir el almacén seguro de credenciales del sistema".to_string())
}

fn save_refresh_token_with_entry(entry: &CredentialEntry, refresh_token: &str) -> Result<(), String> {
    entry
        .set_password(refresh_token)
        .map_err(|_| "El almacén seguro del sistema rechazó la sesión Microsoft".to_string())
}

fn save_microsoft_refresh_token(refresh_token: &str) -> Result<(), String> {
    let _guard = MICROSOFT_CREDENTIAL_LOCK
        .lock()
        .map_err(|_| "El acceso al almacén seguro quedó bloqueado".to_string())?;
    save_refresh_token_with_entry(&microsoft_credential_entry()?, refresh_token)
}

fn load_refresh_token_with_entry(
    entry: &CredentialEntry,
) -> Result<Option<Zeroizing<String>>, String> {
    match entry.get_password() {
        Ok(token) => Ok(Some(Zeroizing::new(token))),
        Err(CredentialError::NoEntry) => Ok(None),
        Err(_) => Err("No se pudo leer la sesión guardada del almacén seguro del sistema".into()),
    }
}

fn load_microsoft_refresh_token() -> Result<Option<Zeroizing<String>>, String> {
    let _guard = MICROSOFT_CREDENTIAL_LOCK
        .lock()
        .map_err(|_| "El acceso al almacén seguro quedó bloqueado".to_string())?;
    load_refresh_token_with_entry(&microsoft_credential_entry()?)
}

fn delete_refresh_token_with_entry(entry: &CredentialEntry) -> Result<(), String> {
    match entry.delete_credential() {
        Ok(()) | Err(CredentialError::NoEntry) => Ok(()),
        Err(_) => Err("No se pudo eliminar la sesión Microsoft del almacén seguro del sistema".into()),
    }
}

fn delete_microsoft_refresh_token() -> Result<(), String> {
    let _guard = MICROSOFT_CREDENTIAL_LOCK
        .lock()
        .map_err(|_| "El acceso al almacén seguro quedó bloqueado".to_string())?;
    delete_refresh_token_with_entry(&microsoft_credential_entry()?)
}

fn set_microsoft_restore_status(app: &AppHandle, notice: Option<String>) -> Result<(), String> {
    let state = app.state::<MicrosoftSessionStatus>();
    let mut status = state
        .0
        .lock()
        .map_err(|_| "El estado de la sesión Microsoft quedó bloqueado".to_string())?;
    status.restore_started = false;
    status.restore_attempted = true;
    status.notice = notice;
    Ok(())
}

fn restore_microsoft_session(app: &AppHandle) -> Option<String> {
    let settings = match read_settings(app) {
        Ok(settings) => settings,
        Err(_) => return Some("No se pudo leer la configuración para restaurar la sesión Microsoft".into()),
    };
    if settings.account_mode != Some(AccountMode::Microsoft) {
        return None;
    }
    let refresh_token = match load_microsoft_refresh_token() {
        Ok(Some(token)) => token,
        Ok(None) => {
            if update_settings(app, |settings| {
                if settings.account_mode == Some(AccountMode::Microsoft) {
                    settings.account_mode = None;
                    Ok(true)
                } else {
                    Ok(false)
                }
            })
            .is_err()
            {
                return Some("No se encontró la sesión Microsoft guardada y no se pudo reparar el perfil".into());
            }
            return Some("No se encontró una sesión Microsoft guardada. Inicia sesión otra vez o elige un nick offline.".into());
        }
        Err(error) => return Some(error),
    };
    let Some(client_id) = configured_microsoft_client_id() else {
        return Some("La sesión Microsoft está guardada, pero esta compilación no tiene habilitado el inicio de sesión".into());
    };
    let account = match complete_refresh(client_id, None, &refresh_token) {
        Ok(account) => account,
        Err(_) => return Some("No se pudo renovar la sesión Microsoft guardada. Inicia sesión otra vez.".into()),
    };
    let persistence_notice = save_microsoft_refresh_token(&account.refresh_token)
        .err()
        .map(|_| "La sesión se recuperó, pero no se pudo guardar el token renovado; quizá debas iniciar sesión otra vez al reiniciar.".to_string());
    let session = AuthenticatedAccount {
        profile: MicrosoftProfile {
            username: account.name,
            uuid: account.id,
        },
        access_token: account.access_token,
        refresh_token: account.refresh_token,
    };
    match app.state::<AuthSession>().0.lock() {
        Ok(mut current) => *current = Some(session),
        Err(_) => return Some("La sesión Microsoft se recuperó, pero el estado interno quedó bloqueado".into()),
    }
    persistence_notice
}

fn restore_microsoft_session_once(app: &AppHandle) -> Result<bool, String> {
    let state = app.state::<MicrosoftSessionStatus>();
    {
        let mut status = state
            .0
            .lock()
            .map_err(|_| "El estado de la sesión Microsoft quedó bloqueado".to_string())?;
        if status.restore_started || status.restore_attempted {
            return Ok(false);
        }
        status.restore_started = true;
    }
    let notice = restore_microsoft_session(app);
    let mut status = state
        .0
        .lock()
        .map_err(|_| "El estado de la sesión Microsoft quedó bloqueado".to_string())?;
    status.restore_started = false;
    status.restore_attempted = true;
    status.notice = notice;
    Ok(true)
}

#[derive(Default)]
struct MinecraftProcess(std::sync::Mutex<MinecraftProcessState>);

#[derive(Default)]
struct ForgeInstallerProcess(std::sync::Mutex<ForgeInstallerState>);

#[derive(Default)]
struct ForgeInstallerState {
    active: Option<std::process::Child>,
    shutting_down: bool,
}

fn ensure_forge_installer_can_start(state: &ForgeInstallerState) -> Result<(), String> {
    if state.shutting_down {
        return Err("El launcher se está cerrando; no se iniciará el instalador Forge".into());
    }
    if state.active.is_some() {
        return Err("Ya hay otra instalación de Forge en curso".into());
    }
    Ok(())
}

fn terminate_registered_forge_installer(process: &ForgeInstallerProcess) -> bool {
    let child = {
        let Ok(mut state) = process.0.lock() else {
            return false;
        };
        state.shutting_down = true;
        state.active.take()
    };
    let Some(mut child) = child else {
        return false;
    };
    terminate_forge_child(&mut child);
    child.wait().is_ok()
}

fn terminate_registered_forge_installer_after_timeout(process: &ForgeInstallerProcess) -> bool {
    let child = {
        let Ok(mut state) = process.0.lock() else {
            return false;
        };
        state.active.take()
    };
    let Some(mut child) = child else {
        return false;
    };
    terminate_forge_child(&mut child);
    child.wait().is_ok()
}

fn terminate_forge_child(child: &mut std::process::Child) {
    #[cfg(unix)]
    unsafe {
        // Forge is started in a dedicated process group so Java wrappers and
        // descendants cannot keep the piped output alive after a timeout.
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = child.kill();
}

fn forge_install_timeout_expired(started: Instant, now: Instant) -> bool {
    now.saturating_duration_since(started) >= FORGE_INSTALL_TIMEOUT
}

fn stop_forge_installer(app: &AppHandle) {
    let process = app.state::<ForgeInstallerProcess>();
    let _ = terminate_registered_forge_installer(&process);
}

fn wait_for_forge_installer(
    app: &AppHandle,
    deadline: Instant,
) -> Result<std::process::ExitStatus, String> {
    loop {
        let process = app.state::<ForgeInstallerProcess>();
        let mut state = process
            .0
            .lock()
            .map_err(|_| "El estado del instalador Forge quedó bloqueado".to_string())?;
        if state.shutting_down {
            return Err("La instalación de Forge se detuvo al cerrar el launcher".into());
        }
        if Instant::now() >= deadline {
            drop(state);
            terminate_registered_forge_installer_after_timeout(&process);
            return Err(format!(
                "La instalación de Forge superó el límite de {} minutos y fue cancelada. Revisa el log de instalación y vuelve a intentarlo.",
                FORGE_INSTALL_TIMEOUT.as_secs() / 60
            ));
        }
        let Some(child) = state.active.as_mut() else {
            return Err("La instalación de Forge se detuvo al cerrar el launcher".into());
        };
        match child.try_wait() {
            Ok(Some(status)) => {
                state.active = None;
                return Ok(status);
            }
            Ok(None) => {}
            Err(error) => return Err(format!("No se pudo consultar el instalador Forge: {error}")),
        }
        drop(state);
        thread::sleep(Duration::from_millis(100));
    }
}

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

#[derive(Clone, Debug, Serialize)]
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
    background_preset: Option<String>,
    java: JavaStatus,
    java_manually_selected: bool,
    managed_game_directories: BTreeMap<String, String>,
    installed_profiles: BTreeMap<String, String>,
    microsoft_login_available: bool,
    session_restore_complete: bool,
    microsoft_session_notice: Option<String>,
    github_developer_enabled: bool,
    developer_github_user: Option<String>,
    microsoft_profile: Option<MicrosoftProfile>,
    offline_username: Option<String>,
    account_mode: Option<AccountMode>,
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
const MAX_SETTINGS_BYTES: u64 = 1_048_576;
const INSTALL_LOG_MAX_MESSAGE_BYTES: usize = 16 * 1024;
static INSTALL_LOG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
static SETTINGS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct InstallLogGuard {
    _process_guard: std::sync::MutexGuard<'static, ()>,
    _system_lock: fs::File,
}

fn install_log_lock_path(path: &Path) -> PathBuf {
    path.with_file_name("launcher.log.lock")
}

fn lock_install_log(path: &Path) -> Result<InstallLogGuard, String> {
    let process_guard = INSTALL_LOG_LOCK
        .lock()
        .map_err(|_| "El bloqueo del registro local dejó de estar disponible".to_string())?;
    let lock_path = install_log_lock_path(path);
    let lock_parent = lock_path
        .parent()
        .ok_or_else(|| "La ruta del registro no contiene una carpeta".to_string())?;
    fs::create_dir_all(lock_parent)
        .map_err(|error| format!("No se pudo crear la carpeta del registro: {error}"))?;
    let system_lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)
        .map_err(|error| format!("No se pudo abrir el bloqueo del registro: {error}"))?;
    system_lock
        .lock()
        .map_err(|error| format!("No se pudo adquirir el bloqueo del registro: {error}"))?;
    Ok(InstallLogGuard {
        _process_guard: process_guard,
        _system_lock: system_lock,
    })
}

struct SettingsFileGuard {
    _process_guard: std::sync::MutexGuard<'static, ()>,
    _system_lock: fs::File,
}

fn settings_lock_path(settings_path: &Path) -> PathBuf {
    settings_path.with_file_name("settings.lock")
}

fn lock_settings_file(settings_path: &Path) -> Result<SettingsFileGuard, String> {
    let process_guard = SETTINGS_LOCK
        .lock()
        .map_err(|_| "El bloqueo de configuración local dejó de estar disponible".to_string())?;
    let lock_path = settings_lock_path(settings_path);
    let lock_parent = lock_path
        .parent()
        .ok_or_else(|| "No se pudo determinar la carpeta de configuración".to_string())?;
    fs::create_dir_all(lock_parent)
        .map_err(|error| format!("No se pudo preparar el bloqueo de configuración: {error}"))?;
    let system_lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(lock_path)
        .map_err(|error| format!("No se pudo abrir el bloqueo de configuración: {error}"))?;
    system_lock
        .lock()
        .map_err(|error| format!("No se pudo adquirir el bloqueo de configuración: {error}"))?;
    Ok(SettingsFileGuard {
        _process_guard: process_guard,
        _system_lock: system_lock,
    })
}

fn is_valid_theme(theme_id: &str) -> bool {
    matches!(
        theme_id,
        "arena-light" | "siege-night" | "ghouls-day" | "outbreak-night"
            | "glacier-light" | "midnight-blue" | "forest-mist" | "deep-forest"
            | "dune-light" | "forge-dark" | "rose-light" | "eclipse"
            | "ocean-light" | "abyss" | "lavender-light" | "carbon"
            // Previous releases may still have these persisted values.
            | "series" | "siege" | "ghouls" | "light"
    )
}

fn resolve_theme(theme_id: Option<String>) -> String {
    match theme_id.as_deref() {
        Some("siege") => "siege-night".into(),
        Some("ghouls") => "outbreak-night".into(),
        Some("series" | "light") | None => "arena-light".into(),
        Some(theme) if is_valid_theme(theme) => theme.to_string(),
        _ => "arena-light".into(),
    }
}

fn migrate_theme_preference(settings: &mut Settings) -> bool {
    if settings.theme_migrated {
        return false;
    }
    if matches!(settings.theme_id.as_deref(), None | Some("series")) {
        settings.theme_id = Some("arena-light".into());
    }
    settings.theme_migrated = true;
    true
}

fn migrate_microsoft_client_id_preference(settings: &mut Settings) -> bool {
    settings.microsoft_client_id.take().is_some()
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
        let id = series.id.to_ascii_lowercase();
        if series.id.is_empty()
            || series.id.len() > 64
            || !series.id.bytes().all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_'))
            || !ids.insert(id)
        {
            return Err("El catálogo contiene identificadores de serie vacíos o duplicados".into());
        }
        if !valid_catalog_text(&series.name, 80)
            || !valid_catalog_text(&series.subtitle, 100)
            || !valid_catalog_text(&series.description, 600)
        {
            return Err(format!("La serie {} contiene texto vacío, demasiado largo o con controles", series.id));
        }
        if !valid_catalog_color(&series.accent) {
            return Err(format!("El color de la serie {} no es un hexadecimal #RRGGBB válido", series.id));
        }
        if !valid_numeric_version(&series.minecraft_version, 2, 3)
            || !series.loader.eq_ignore_ascii_case("forge")
            || !valid_numeric_version(&series.loader_version, 2, 4)
        {
            return Err(format!("La serie {} no define una versión compatible de Minecraft y Forge", series.id));
        }
    }
    Ok(catalog)
}

fn valid_catalog_text(value: &str, max_chars: usize) -> bool {
    let trimmed = value.trim();
    !trimmed.is_empty()
        && trimmed.chars().count() <= max_chars
        && !trimmed.chars().any(char::is_control)
}

fn valid_catalog_color(value: &str) -> bool {
    value.len() == 7
        && value.starts_with('#')
        && value[1..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_numeric_version(value: &str, min_parts: usize, max_parts: usize) -> bool {
    let parts: Vec<&str> = value.split('.').collect();
    (min_parts..=max_parts).contains(&parts.len())
        && parts.iter().all(|part| {
            !part.is_empty()
                && part.len() <= 6
                && part.bytes().all(|byte| byte.is_ascii_digit())
                && (*part == "0" || !part.starts_with('0'))
        })
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

fn catalog_refresh_due(
    now: Instant,
    last_attempt: Option<Instant>,
    has_cached_catalog: bool,
    currently_online: bool,
) -> bool {
    let interval = if has_cached_catalog && currently_online {
        CATALOG_REFRESH_INTERVAL
    } else {
        CATALOG_RETRY_INTERVAL
    };
    last_attempt.is_none_or(|last| now.saturating_duration_since(last) >= interval)
}

fn read_bounded_body(reader: impl Read, max_bytes: usize) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(max_bytes.min(8192));
    reader
        .take(max_bytes as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "response body exceeds the configured size limit",
        ));
    }
    Ok(bytes)
}

fn read_catalog_cache(path: &Path) -> Option<Catalog> {
    let file = fs::File::open(path).ok()?;
    let bytes = read_bounded_body(file, MAX_CATALOG_BYTES).ok()?;
    let catalog: Catalog = serde_json::from_slice(&bytes).ok()?;
    validate_catalog(catalog).ok()
}

fn load_runtime_catalog_cache(cache_path: &Path) -> Result<(), String> {
    let runtime_catalog = RUNTIME_CATALOG.get_or_init(|| RwLock::new(None));
    let mut cached_catalog = runtime_catalog
        .write()
        .map_err(|_| "El catálogo local quedó bloqueado".to_string())?;
    if cached_catalog.is_none() {
        *cached_catalog = read_catalog_cache(cache_path);
    }
    Ok(())
}

fn write_catalog_cache(path: &Path, catalog: &Catalog) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "La ruta de caché del catálogo no tiene carpeta".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("No se pudo preparar la caché del catálogo: {error}"))?;
    let bytes = serde_json::to_vec(catalog).map_err(|error| error.to_string())?;
    if bytes.len() > MAX_CATALOG_BYTES {
        return Err("El catálogo no se guardó porque excede 1 MiB".into());
    }
    let mut file = AtomicWriteFile::options()
        .open(path)
        .map_err(|error| format!("No se pudo preparar la caché del catálogo: {error}"))?;
    file.write_all(&bytes)
        .map_err(|error| format!("No se pudo escribir la caché del catálogo: {error}"))?;
    file.commit()
        .map_err(|error| format!("No se pudo guardar la caché del catálogo: {error}"))
}

fn refresh_runtime_catalog(cache_path: &Path) -> Result<(), String> {
    let mut last_attempt = RUNTIME_CATALOG_LAST_ATTEMPT
        .get_or_init(|| std::sync::Mutex::new(None))
        .lock()
        .map_err(|_| "El estado de actualización del catálogo quedó bloqueado".to_string())?;
    let runtime_catalog = RUNTIME_CATALOG.get_or_init(|| RwLock::new(None));
    load_runtime_catalog_cache(cache_path)?;
    let has_cached_catalog = runtime_catalog
        .read()
        .map_err(|_| "El catálogo local quedó bloqueado".to_string())?
        .is_some();
    let now = Instant::now();
    let currently_online = RUNTIME_CATALOG_ONLINE.load(std::sync::atomic::Ordering::Relaxed);
    if !catalog_refresh_due(now, *last_attempt, has_cached_catalog, currently_online) {
        return Ok(());
    }
    *last_attempt = Some(now);
    RUNTIME_CATALOG_ONLINE.store(false, std::sync::atomic::Ordering::Relaxed);
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
    if response
        .content_length()
        .is_some_and(|length| length > MAX_CATALOG_BYTES as u64)
    {
        return Err("El catálogo remoto excede 1 MiB".into());
    }
    let bytes = read_bounded_body(response, MAX_CATALOG_BYTES)
        .map_err(|error| format!("No se pudo leer el catálogo remoto: {error}"))?;
    let catalog: Catalog = serde_json::from_slice(&bytes)
        .map_err(|error| format!("El catálogo remoto no es válido: {error}"))?;
    let catalog = validate_catalog(catalog)?;
    *runtime_catalog.write()
        .map_err(|_| "El catálogo local quedó bloqueado".to_string())? = Some(catalog);
    RUNTIME_CATALOG_ONLINE.store(true, std::sync::atomic::Ordering::Relaxed);
    let catalog = runtime_catalog
        .read()
        .map_err(|_| "El catálogo local quedó bloqueado".to_string())?
        .clone()
        .expect("catalog was just stored");
    write_catalog_cache(cache_path, &catalog)?;
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

fn read_settings_file(path: &Path) -> Result<(Settings, Option<PathBuf>), String> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Settings::default(), None));
        }
        Err(error) => return Err(format!("No se pudo leer {}: {error}", path.display())),
    };
    let mut bytes = Vec::with_capacity((MAX_SETTINGS_BYTES + 1) as usize);
    file.take(MAX_SETTINGS_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("No se pudo leer {}: {error}", path.display()))?;
    let parsed = if bytes.len() as u64 > MAX_SETTINGS_BYTES {
        Err(format!("la configuración supera el límite de 1 MiB"))
    } else {
        serde_json::from_slice(&bytes).map_err(|error| error.to_string())
    };
    match parsed {
        Ok(settings) => Ok((settings, None)),
        Err(parse_error) => {
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            let filename = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();
            let backup = path.with_file_name(format!("{filename}.corrupt-{timestamp}"));
            fs::rename(path, &backup).map_err(|rename_error| {
                format!(
                    "La configuración {} no es válida ({parse_error}) y no se pudo conservar una copia en {}: {rename_error}",
                    path.display(),
                    backup.display()
                )
            })?;
            let defaults = Settings::default();
            write_settings_file(path, &defaults)?;
            Ok((defaults, Some(backup)))
        }
    }
}

fn read_settings(app: &AppHandle) -> Result<Settings, String> {
    let path = settings_path(app)?;
    let _guard = lock_settings_file(&path)?;
    let (settings, backup) = read_settings_file(&path)?;
    drop(_guard);
    if let Some(backup) = backup {
        let _ = append_install_log(
            app,
            "launcher",
            "Configuración",
            &format!(
                "La configuración dañada se conservó en {}; el launcher restauró valores predeterminados.",
                backup.display()
            ),
        );
    }
    Ok(settings)
}

fn write_settings_file(path: &Path, settings: &Settings) -> Result<(), String> {
    let bytes = serde_json::to_vec_pretty(settings).map_err(|error| error.to_string())?;
    let mut file = AtomicWriteFile::options()
        .open(path)
        .map_err(|error| format!("No se pudo preparar la configuración: {error}"))?;
    file.write_all(&bytes)
        .map_err(|error| format!("No se pudo escribir la configuración: {error}"))?;
    file.commit()
        .map_err(|error| format!("No se pudo confirmar la configuración: {error}"))
}

fn update_settings_file(
    path: &Path,
    update: impl FnOnce(&mut Settings) -> Result<bool, String>,
) -> Result<(Settings, Option<PathBuf>), String> {
    let _guard = lock_settings_file(path)?;
    let (mut settings, backup) = read_settings_file(path)?;
    if update(&mut settings)? {
        write_settings_file(path, &settings)?;
    }
    Ok((settings, backup))
}

fn update_settings(
    app: &AppHandle,
    update: impl FnOnce(&mut Settings) -> Result<bool, String>,
) -> Result<Settings, String> {
    let path = settings_path(app)?;
    let (settings, backup) = update_settings_file(&path, update)?;
    if let Some(backup) = backup {
        let _ = append_install_log(
            app,
            "launcher",
            "Configuración",
            &format!(
                "La configuración dañada se conservó en {}; el launcher restauró valores predeterminados.",
                backup.display()
            ),
        );
    }
    Ok(settings)
}

fn valid_game_dir(path: &Path) -> Result<PathBuf, String> {
    if !path.is_dir() {
        return Err("La carpeta seleccionada ya no existe o no es un directorio".into());
    }
    path.canonicalize()
        .map_err(|error| format!("No se pudo resolver la carpeta seleccionada: {error}"))
}

fn game_directories_overlap(left: &Path, right: &Path) -> bool {
    #[cfg(windows)]
    {
        // Windows normalmente resuelve rutas sin distinguir mayúsculas. Normalizar
        // antes de comparar evita asignar la misma instancia dos veces con casing distinto.
        let left = left.to_string_lossy().replace('/', "\\").to_lowercase();
        let right = right.to_string_lossy().replace('/', "\\").to_lowercase();
        let left = Path::new(&left);
        let right = Path::new(&right);
        left.starts_with(right) || right.starts_with(left)
    }
    #[cfg(not(windows))]
    {
        left.starts_with(right) || right.starts_with(left)
    }
}

fn ensure_game_directory_isolated(
    app: &AppHandle,
    settings: &Settings,
    series_id: &str,
    game_directory: &Path,
) -> Result<(), String> {
    let game_directory = valid_game_dir(game_directory)?;
    let catalog = current_catalog()?;

    let mut other_directories = Vec::new();
    for (other_id, configured_path) in &settings.game_directories {
        if other_id != series_id {
            if let Ok(path) = valid_game_dir(Path::new(configured_path)) {
                other_directories.push((other_id.clone(), path));
            }
        }
    }

    let local_data = app
        .path()
        .app_local_data_dir()
        .map_err(|error| error.to_string())?;
    let roaming_data = app
        .path()
        .app_data_dir()
        .map_err(|error| error.to_string())?;
    for other in catalog.series.iter().filter(|other| other.id != series_id) {
        let managed = resolve_managed_instance_dir(&local_data, &roaming_data, &other.id)?;
        // Incluir también rutas aún no creadas: un directorio elegido como padre
        // de una futura instancia administrada podría mezclar o borrar su pack.
        let path = if managed.is_dir() {
            valid_game_dir(&managed)?
        } else {
            managed
        };
        other_directories.push((other.id.clone(), path));
    }

    if let Some((other_id, _)) = other_directories
        .into_iter()
        .find(|(_, other)| game_directories_overlap(&game_directory, other))
    {
        return Err(format!(
            "La carpeta de juego se superpone con la instancia de {other_id}. Asigna una carpeta independiente a cada serie."
        ));
    }
    Ok(())
}

fn resolve_isolated_series_game_directory(
    app: &AppHandle,
    series_id: &str,
) -> Result<Option<PathBuf>, String> {
    let game_directory = resolve_series_game_directory(app, series_id)?;
    if let Some(path) = game_directory.as_deref() {
        let settings = read_settings(app)?;
        ensure_game_directory_isolated(app, &settings, series_id, path)?;
    }
    Ok(game_directory)
}

fn managed_game_dir(app: &AppHandle, series_id: &str) -> Result<PathBuf, String> {
    let catalog = current_catalog()?;
    if !catalog.series.iter().any(|series| series.id == series_id) {
        return Err("La serie solicitada no existe en el catálogo".into());
    }
    let local_data = app
        .path()
        .app_local_data_dir()
        .map_err(|error| error.to_string())?;
    let roaming_data = app
        .path()
        .app_data_dir()
        .map_err(|error| error.to_string())?;
    resolve_managed_instance_dir(&local_data, &roaming_data, series_id)
}

fn resolve_managed_instance_dir(
    local_app_data: &Path,
    legacy_roaming_app_data: &Path,
    series_id: &str,
) -> Result<PathBuf, String> {
    let local = local_app_data.join("instances").join(series_id);
    if local.is_dir() {
        return Ok(local);
    }
    if local.exists() {
        return Err(format!(
            "La ruta administrada de {series_id} existe y no es una carpeta: {}",
            local.display()
        ));
    }

    let legacy = legacy_roaming_app_data.join("instances").join(series_id);
    if legacy.is_dir() { Ok(legacy) } else { Ok(local) }
}

// Call only while holding the pack lock in the roaming app-data directory so
// older launcher processes cannot write into the legacy instance mid-move.
fn migrate_managed_instance_dir(
    local_app_data: &Path,
    legacy_roaming_app_data: &Path,
    series_id: &str,
) -> Result<PathBuf, String> {
    let current = resolve_managed_instance_dir(local_app_data, legacy_roaming_app_data, series_id)?;
    let local = local_app_data.join("instances").join(series_id);
    if current == local {
        return Ok(local);
    }
    let legacy = current;
    if find_running_minecraft_pid(&legacy).is_some() {
        return Ok(legacy);
    }

    let parent = local
        .parent()
        .ok_or_else(|| "No se pudo resolver la carpeta local de instancias".to_string())?;
    // Roaming AppData is not suitable for large Minecraft instances on Windows.
    // Move existing installs locally on the same volume; if Windows cannot move
    // an open/locked instance, keep using its original directory without deleting it.
    if fs::create_dir_all(parent).is_err() {
        return Ok(legacy);
    }
    match fs::rename(&legacy, &local) {
        Ok(()) => Ok(local),
        Err(_) if local.is_dir() => Ok(local),
        Err(_) if legacy.is_dir() => Ok(legacy),
        Err(_) => Ok(local),
    }
}

fn migrate_managed_instance_for_series(
    app: &AppHandle,
    series_id: &str,
) -> Result<PathBuf, String> {
    let local_data = app
        .path()
        .app_local_data_dir()
        .map_err(|error| error.to_string())?;
    let roaming_data = app
        .path()
        .app_data_dir()
        .map_err(|error| error.to_string())?;
    migrate_managed_instance_dir(&local_data, &roaming_data, series_id)
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

fn retry_request<T, E>(
    attempts: usize,
    delay: Duration,
    mut request: impl FnMut() -> Result<T, E>,
) -> Result<T, E> {
    let attempts = attempts.max(1);
    let mut last_error = None;
    for attempt in 0..attempts {
        match request() {
            Ok(value) => return Ok(value),
            Err(error) => {
                last_error = Some(error);
                if attempt + 1 < attempts && !delay.is_zero() {
                    thread::sleep(delay);
                }
            }
        }
    }
    Err(last_error.expect("at least one request attempt is made"))
}

fn fetch_forge_checksum_sidecar(installer_url: &str) -> Result<String, String> {
    let sidecar_url = format!("{installer_url}.sha1");
    retry_request(
        FORGE_METADATA_ATTEMPTS,
        FORGE_METADATA_RETRY_DELAY,
        || http::get_text(&sidecar_url).map_err(|error| error.to_string()),
    )
    .map_err(|error| {
        format!(
            "No se pudo descargar el checksum de Forge tras {FORGE_METADATA_ATTEMPTS} intentos: {error}"
        )
    })
}

struct AppInstallProgress {
    app: AppHandle,
    series_id: String,
    stage: String,
    completed_files: u64,
    current_file: String,
    last_byte_event: Option<Instant>,
    last_byte_log: Option<Instant>,
}

impl AppInstallProgress {
    fn new(app: AppHandle, series_id: String) -> Self {
        Self {
            app,
            series_id,
            stage: "Preparando".into(),
            completed_files: 0,
            current_file: String::new(),
            last_byte_event: None,
            last_byte_log: None,
        }
    }

    fn set_stage(&mut self, stage: &str, message: &str) {
        self.stage = stage.into();
        self.emit(message);
    }

    fn emit(&self, message: &str) {
        let _ = append_install_log(&self.app, &self.series_id, &self.stage, message);
        self.emit_progress(message);
    }

    fn emit_progress(&self, message: &str) {
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

fn progress_update_due(
    last_update: Option<Instant>,
    now: Instant,
    interval: Duration,
    force: bool,
) -> bool {
    force || last_update.is_none_or(|last| now.duration_since(last) >= interval)
}

fn append_install_log(
    app: &AppHandle,
    series_id: &str,
    stage: &str,
    message: &str,
) -> Result<(), String> {
    let app_data = app
        .path()
        .app_local_data_dir()
        .map_err(|error| format!("No se pudo ubicar el registro local: {error}"))?;
    let path = app_data.join("logs").join("launcher.log");
    append_install_log_at(&path, series_id, stage, message)
}

fn sanitize_install_log_message(message: &str) -> String {
    let mut safe = String::with_capacity(message.len().min(INSTALL_LOG_MAX_MESSAGE_BYTES));
    let mut truncated = false;
    for character in message.chars() {
        let character = if matches!(character, '\n' | '\r') {
            ' '
        } else {
            character
        };
        if safe.len() + character.len_utf8() > INSTALL_LOG_MAX_MESSAGE_BYTES {
            truncated = true;
            break;
        }
        safe.push(character);
    }
    if truncated {
        const MARKER: &str = "…[truncado]";
        let content_limit = INSTALL_LOG_MAX_MESSAGE_BYTES - MARKER.len();
        while safe.len() > content_limit {
            safe.pop();
        }
        safe.push_str(MARKER);
    }
    safe
}

fn append_install_log_at(
    path: &Path,
    series_id: &str,
    stage: &str,
    message: &str,
) -> Result<(), String> {
    let parent = path
        .parent()
        .ok_or_else(|| "La ruta del registro no contiene una carpeta".to_string())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("No se pudo crear la carpeta del registro: {error}"))?;
    let _guard = lock_install_log(path)?;

    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let line = format!(
        "{seconds} [{series_id}] [{stage}] {}\n",
        sanitize_install_log_message(message)
    );
    if fs::metadata(path).is_ok_and(|metadata| {
        metadata
            .len()
            .saturating_add(line.len() as u64)
            > INSTALL_LOG_MAX_BYTES
    }) {
        let previous = path.with_extension("log.1");
        let _ = fs::remove_file(&previous);
        fs::rename(path, previous)
            .map_err(|error| format!("No se pudo rotar el registro local: {error}"))?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|error| format!("No se pudo abrir el registro local: {error}"))?;
    file.write_all(line.as_bytes())
        .map_err(|error| format!("No se pudo escribir el registro local: {error}"))
}

fn read_install_log_at(path: &Path) -> Result<String, String> {
    const DISPLAY_LIMIT: u64 = 256 * 1024;
    let _guard = lock_install_log(path)?;
    read_log_tail(path, DISPLAY_LIMIT)
        .map(|content| content.unwrap_or_else(|| "Todavía no hay registros de instalación.".into()))
}

fn read_log_tail(path: &Path, max_bytes: u64) -> Result<Option<String>, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => metadata,
        Ok(_) => return Err("El registro no es un archivo normal".into()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("No se pudo revisar el registro: {error}")),
    };
    let mut file = fs::File::open(path)
        .map_err(|error| format!("No se pudo abrir el registro: {error}"))?;
    let start = metadata.len().saturating_sub(max_bytes);
    let starts_at_line_boundary = if start == 0 {
        true
    } else {
        file.seek(SeekFrom::Start(start - 1))
            .map_err(|error| format!("No se pudo buscar el final del registro: {error}"))?;
        let mut previous_byte = [0_u8; 1];
        file.read_exact(&mut previous_byte)
            .map_err(|error| format!("No se pudo leer el registro: {error}"))?;
        previous_byte[0] == b'\n'
    };
    file.seek(SeekFrom::Start(start))
        .map_err(|error| format!("No se pudo buscar el final del registro: {error}"))?;
    let mut bytes = Vec::with_capacity(metadata.len().saturating_sub(start) as usize);
    file.take(max_bytes)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("No se pudo leer el registro: {error}"))?;
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

fn read_latest_crash_report(game_directory: &Path) -> Result<Option<(String, String)>, String> {
    const CRASH_REPORT_DISPLAY_LIMIT: u64 = 128 * 1024;
    let crash_directory = game_directory.join("crash-reports");
    match fs::symlink_metadata(&crash_directory) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => return Err("La carpeta de informes de Minecraft no es un directorio normal".into()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("No se pudo revisar los informes de Minecraft: {error}")),
    }

    let entries = fs::read_dir(&crash_directory)
        .map_err(|error| format!("No se pudieron listar los informes de Minecraft: {error}"))?;
    let mut latest: Option<(std::time::SystemTime, String, PathBuf)> = None;
    for entry in entries {
        let entry = entry.map_err(|error| format!("No se pudo leer un informe de Minecraft: {error}"))?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !name.starts_with("crash-") || !name.ends_with(".txt") {
            continue;
        }
        if !entry
            .file_type()
            .map_err(|error| format!("No se pudo revisar el informe {name}: {error}"))?
            .is_file()
        {
            continue;
        }
        let metadata = entry
            .metadata()
            .map_err(|error| format!("No se pudo revisar el informe {name}: {error}"))?;
        let modified = metadata.modified().unwrap_or(std::time::UNIX_EPOCH);
        let path = entry.path();
        let candidate = (modified, name, path);
        if latest.as_ref().is_none_or(|current| {
            candidate.0 > current.0 || (candidate.0 == current.0 && candidate.1 > current.1)
        }) {
            latest = Some(candidate);
        }
    }

    let Some((_, name, path)) = latest else {
        return Ok(None);
    };
    let Some(contents) = read_log_tail(&path, CRASH_REPORT_DISPLAY_LIMIT)? else {
        return Ok(None);
    };
    Ok(Some((name, contents)))
}


fn read_latest_jvm_fatal_log(game_directory: &Path) -> Result<Option<(String, String)>, String> {
    const JVM_FATAL_LOG_DISPLAY_LIMIT: u64 = 128 * 1024;
    let entries = match fs::read_dir(game_directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("No se pudieron revisar los registros fatales de Java: {error}")),
    };
    let mut latest: Option<(std::time::SystemTime, String, PathBuf)> = None;
    for entry in entries {
        let entry = entry.map_err(|error| format!("No se pudo leer un registro fatal de Java: {error}"))?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else { continue; };
        if !name.starts_with("hs_err_pid") || !name.ends_with(".log") { continue; }
        if !entry.file_type().map_err(|error| format!("No se pudo revisar el registro {name}: {error}"))?.is_file() {
            continue;
        }
        let metadata = entry.metadata().map_err(|error| format!("No se pudo revisar el registro {name}: {error}"))?;
        let modified = metadata.modified().unwrap_or(std::time::UNIX_EPOCH);
        let candidate = (modified, name, entry.path());
        if latest.as_ref().is_none_or(|current| candidate.0 > current.0 || (candidate.0 == current.0 && candidate.1 > current.1)) {
            latest = Some(candidate);
        }
    }
    let Some((_, name, path)) = latest else { return Ok(None); };
    let Some(contents) = read_log_tail(&path, JVM_FATAL_LOG_DISPLAY_LIMIT)? else { return Ok(None); };
    Ok(Some((name, contents)))
}

#[cfg(test)]
mod settings_recovery_tests {
    use super::*;

    #[test]
    fn settings_file_lock_blocks_another_handle_and_releases_on_drop() {
        let fixture = std::env::temp_dir().join(format!(
            "eternalcraft-settings-lock-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&fixture).unwrap();
        let path = fixture.join("settings.json");
        let first = lock_settings_file(&path).unwrap();
        let second = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(settings_lock_path(&path))
            .unwrap();

        assert!(matches!(
            second.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        drop(first);
        assert!(second.try_lock().is_ok());
        drop(second);
        fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn concurrent_settings_updates_do_not_overwrite_each_other() {
        let fixture = std::env::temp_dir().join(format!(
            "eternalcraft-settings-concurrent-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&fixture).unwrap();
        let path = fixture.join("settings.json");
        write_settings_file(&path, &Settings::default()).unwrap();

        let (first_entered_tx, first_entered_rx) = std::sync::mpsc::channel();
        let (release_first_tx, release_first_rx) = std::sync::mpsc::channel();
        let first_path = path.clone();
        let first = thread::spawn(move || {
            update_settings_file(&first_path, |settings| {
                first_entered_tx.send(()).unwrap();
                release_first_rx.recv().unwrap();
                settings.active_series_id = Some("siege".into());
                Ok(true)
            })
        });
        first_entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("first update should enter its transaction");

        let (second_started_tx, second_started_rx) = std::sync::mpsc::channel();
        let (second_entered_tx, second_entered_rx) = std::sync::mpsc::channel();
        let second_path = path.clone();
        let second = thread::spawn(move || {
            second_started_tx.send(()).unwrap();
            update_settings_file(&second_path, |settings| {
                second_entered_tx.send(()).unwrap();
                settings.theme_id = Some("siege-night".into());
                Ok(true)
            })
        });
        second_started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("second update should start");
        assert!(second_entered_rx.recv_timeout(Duration::from_millis(50)).is_err());

        release_first_tx.send(()).unwrap();
        first.join().unwrap().unwrap();
        second_entered_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("second update should run after the first commits");
        second.join().unwrap().unwrap();

        let (saved, _) = read_settings_file(&path).unwrap();
        assert_eq!(saved.active_series_id.as_deref(), Some("siege"));
        assert_eq!(saved.theme_id.as_deref(), Some("siege-night"));
        fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn oversized_settings_are_preserved_and_replaced_with_valid_defaults() {
        let fixture = std::env::temp_dir().join(format!(
            "eternalcraft-settings-size-limit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&fixture).unwrap();
        let path = fixture.join("settings.json");
        let oversized = vec![b'x'; MAX_SETTINGS_BYTES as usize + 32];
        fs::write(&path, &oversized).unwrap();

        let (settings, backup) = read_settings_file(&path).unwrap();
        let backup = backup.expect("oversized settings should be preserved");
        assert_eq!(fs::read(&backup).unwrap(), oversized);
        assert!(settings.active_series_id.is_none());
        assert!(settings.game_directories.is_empty());
        let saved: Settings = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(saved.active_series_id.is_none());

        fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn malformed_settings_are_preserved_and_replaced_with_valid_defaults() {
        let fixture = std::env::temp_dir().join(format!(
            "eternalcraft-settings-recovery-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&fixture).unwrap();
        let path = fixture.join("settings.json");
        let malformed = b"{ this is not valid settings json";
        fs::write(&path, malformed).unwrap();

        let (settings, backup) = read_settings_file(&path).unwrap();
        let backup = backup.expect("malformed settings should be preserved");
        assert_eq!(fs::read(&backup).unwrap(), malformed);
        assert!(settings.active_series_id.is_none());
        assert!(settings.game_directories.is_empty());
        let saved: Settings = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert!(saved.active_series_id.is_none());
        assert!(read_settings_file(&path).unwrap().1.is_none());

        fs::remove_dir_all(fixture).unwrap();
    }
}

#[cfg(test)]
mod log_tests {
    use super::*;

    #[test]
    fn install_log_lock_blocks_another_handle_and_releases_on_drop() {
        let fixture = std::env::temp_dir().join(format!(
            "eternalcraft-log-lock-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&fixture).unwrap();
        let log = fixture.join("launcher.log");
        let first = lock_install_log(&log).unwrap();
        let second = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(install_log_lock_path(&log))
            .unwrap();

        assert!(matches!(
            second.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        drop(first);
        assert!(second.try_lock().is_ok());
        drop(second);
        fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn oversized_log_messages_are_truncated_before_writing() {
        let fixture = std::env::temp_dir().join(format!(
            "eternalcraft-log-message-limit-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&fixture).unwrap();
        let log = fixture.join("launcher.log");
        let message = "x".repeat(INSTALL_LOG_MAX_MESSAGE_BYTES * 2);

        append_install_log_at(&log, "siege", "Network", &message).unwrap();

        let contents = fs::read_to_string(&log).unwrap();
        assert!(contents.len() <= INSTALL_LOG_MAX_MESSAGE_BYTES + 128);
        assert!(contents.contains("…[truncado]"));
        assert!(contents.ends_with('\n'));
        fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn log_rotates_before_the_next_line_would_exceed_the_limit() {
        let fixture = std::env::temp_dir().join(format!(
            "eternalcraft-log-rotation-bound-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&fixture).unwrap();
        let log = fixture.join("launcher.log");
        fs::write(&log, vec![b'x'; INSTALL_LOG_MAX_BYTES as usize - 8]).unwrap();

        append_install_log_at(&log, "siege", "Network", "line").unwrap();

        assert_eq!(
            fs::metadata(fixture.join("launcher.log.1")).unwrap().len(),
            INSTALL_LOG_MAX_BYTES - 8
        );
        assert!(fs::metadata(&log).unwrap().len() < INSTALL_LOG_MAX_BYTES);
        fs::remove_dir_all(fixture).unwrap();
    }

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

    #[test]
    fn support_selects_only_the_latest_regular_minecraft_crash_report() {
        let fixture = std::env::temp_dir().join(format!(
            "eternalcraft-crash-report-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let crash_directory = fixture.join("crash-reports");
        fs::create_dir_all(&crash_directory).unwrap();
        fs::write(
            crash_directory.join("crash-2026-10-19_10.00.00-client.txt"),
            "older report",
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(20));
        fs::write(
            crash_directory.join("crash-2026-10-20_10.00.00-client.txt"),
            "latest report",
        )
        .unwrap();
        fs::write(crash_directory.join("notes.txt"), "not a Minecraft crash report").unwrap();

        let (name, contents) = read_latest_crash_report(&fixture).unwrap().unwrap();
        assert_eq!(name, "crash-2026-10-20_10.00.00-client.txt");
        assert_eq!(contents, "latest report");
        fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn support_ignores_a_symlinked_crash_report() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let fixture = std::env::temp_dir().join(format!(
                "eternalcraft-crash-report-symlink-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let crash_directory = fixture.join("crash-reports");
            fs::create_dir_all(&crash_directory).unwrap();
            let outside = fixture.join("outside.txt");
            fs::write(&outside, "must not be read").unwrap();
            symlink(&outside, crash_directory.join("crash-2026-10-20_10.00.00-client.txt"))
                .unwrap();

            assert!(read_latest_crash_report(&fixture).unwrap().is_none());
            fs::remove_dir_all(fixture).unwrap();
        }
    }

    #[test]
    fn support_selects_only_the_latest_jvm_fatal_log() {
        let fixture = std::env::temp_dir().join(format!("eternalcraft-jvm-fatal-log-{}-{}",
            std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        fs::create_dir_all(&fixture).unwrap();
        fs::write(fixture.join("hs_err_pid100.log"), "older fatal log").unwrap();
        std::thread::sleep(Duration::from_millis(20));
        fs::write(fixture.join("hs_err_pid200.log"), "latest fatal log").unwrap();
        fs::write(fixture.join("notes.log"), "not a JVM fatal log").unwrap();
        let (name, contents) = read_latest_jvm_fatal_log(&fixture).unwrap().unwrap();
        assert_eq!(name, "hs_err_pid200.log");
        assert_eq!(contents, "latest fatal log");
        fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn support_ignores_a_symlinked_jvm_fatal_log() {
        #[cfg(unix)] {
            use std::os::unix::fs::symlink;
            let fixture = std::env::temp_dir().join(format!("eternalcraft-jvm-fatal-symlink-{}-{}",
                std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
            fs::create_dir_all(&fixture).unwrap();
            let outside = fixture.join("outside.log");
            fs::write(&outside, "must not be read").unwrap();
            symlink(&outside, fixture.join("hs_err_pid200.log")).unwrap();
            assert!(read_latest_jvm_fatal_log(&fixture).unwrap().is_none());
            fs::remove_dir_all(fixture).unwrap();
        }
    }

    #[test]
    fn install_log_reads_a_bounded_tail_without_partial_lines() {
        let path = std::env::temp_dir().join(format!(
            "eternalcraft-install-log-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let contents = (0..40_000)
            .map(|line| format!("installation log line {line:05}\n"))
            .collect::<String>();
        fs::write(&path, contents).unwrap();

        let tail = read_install_log_at(&path).unwrap();
        assert!(tail.len() <= 256 * 1024);
        assert!(tail.starts_with("installation log line "));
        assert!(tail.ends_with("installation log line 39999\n"));
        assert!(!tail.contains("installation log line 00000\n"));

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn download_progress_is_throttled_but_emits_completion_immediately() {
        let start = Instant::now();
        let interval = Duration::from_millis(200);

        assert!(progress_update_due(None, start, interval, false));
        assert!(!progress_update_due(
            Some(start),
            start + Duration::from_millis(100),
            interval,
            false,
        ));
        assert!(progress_update_due(
            Some(start),
            start + interval,
            interval,
            false,
        ));
        assert!(progress_update_due(
            Some(start),
            start + Duration::from_millis(1),
            interval,
            true,
        ));
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
                let now = Instant::now();
                let finished = total.is_some_and(|total| received >= total);
                if progress_update_due(
                    self.last_byte_event,
                    now,
                    Duration::from_millis(200),
                    finished,
                ) {
                    self.emit_progress(&message);
                    self.last_byte_event = Some(now);
                }
                if progress_update_due(
                    self.last_byte_log,
                    now,
                    Duration::from_secs(10),
                    false,
                ) {
                    let _ = append_install_log(&self.app, &self.series_id, &self.stage, &message);
                    self.last_byte_log = Some(now);
                }
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
    let installer_app = app.clone();
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
        mojang_runtime::install_java_runtime(&runtime.component, &game_dir, &mut progress)?;
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
        let checksum = fetch_forge_checksum_sidecar(&installer_url)?;
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
        ensure_forge_launcher_profiles(&game_dir)?;
        let invocation = InstallerInvocation {
            loader: LoaderKind::Forge,
            java_executable: java_executable.clone(),
            installer_path: installer_path.clone(),
            minecraft_dir: game_dir.clone(),
        };
        // Serializar el spawn con el cierre: Exit no puede comprobar el estado
        // vacío entre el arranque del hijo y su registro.
        let process = installer_app.state::<ForgeInstallerProcess>();
        let mut installer_state = process
            .0
            .lock()
            .map_err(|_| "El estado del instalador Forge quedó bloqueado".to_string())?;
        ensure_forge_installer_can_start(&installer_state)?;
        let mut command = Command::new(&invocation.java_executable);
        hide_console_if_windows(&mut command);
        command
            .args(installer_command_args(&invocation))
            .current_dir(&game_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command
            .spawn()
            .map_err(|error| format!("No se pudo iniciar el instalador Forge: {error}"))?;
        let Some(stdout) = child.stdout.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("No se pudo capturar la salida del instalador Forge".into());
        };
        let Some(stderr) = child.stderr.take() else {
            let _ = child.kill();
            let _ = child.wait();
            return Err("No se pudo capturar los errores del instalador Forge".into());
        };
        installer_state.active = Some(child);
        drop(installer_state);
        let (sender, receiver) = mpsc::sync_channel(32);
        let stdout_reader = forward_forge_output(stdout, "stdout", sender.clone());
        let stderr_reader = forward_forge_output(stderr, "stderr", sender);
        let mut recent_output = VecDeque::with_capacity(20);
        let mut last_report = None;
        let started = Instant::now();
        let deadline = started + FORGE_INSTALL_TIMEOUT;
        let mut timed_out = false;
        loop {
            match receiver.recv_timeout(Duration::from_millis(250)) {
                Ok(line) => {
                    if recent_output.len() == 20 {
                        recent_output.pop_front();
                    }
                    recent_output.push_back(line.clone());
                    let now = Instant::now();
                    if progress_update_due(last_report, now, Duration::from_millis(500), false) {
                        progress.emit(&line);
                        last_report = Some(now);
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout)
                    if !timed_out && forge_install_timeout_expired(started, Instant::now()) =>
                {
                    timed_out = true;
                    let process = installer_app.state::<ForgeInstallerProcess>();
                    terminate_registered_forge_installer_after_timeout(&process);
                    let timeout_message = format!(
                        "El instalador Forge superó el límite de {} minutos y fue cancelado.",
                        FORGE_INSTALL_TIMEOUT.as_secs() / 60
                    );
                    let _ = append_install_log(
                        &progress.app,
                        &progress.series_id,
                        "Forge",
                        &timeout_message,
                    );
                    progress.emit("Forge tardó demasiado; se canceló la instalación y puedes volver a intentarlo.");
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        let _ = stdout_reader.join();
        let _ = stderr_reader.join();
        if timed_out {
            return Err(format!("La instalación de Forge superó el límite de {} minutos y fue cancelada. Revisa el log de instalación y vuelve a intentarlo.", FORGE_INSTALL_TIMEOUT.as_secs() / 60));
        }
        let status = match wait_for_forge_installer(&installer_app, deadline) {
            Ok(status) => status,
            Err(error) => {
                if error.contains("superó el límite") {
                    let _ = append_install_log(
                        &progress.app,
                        &progress.series_id,
                        "Forge",
                        &error,
                    );
                    progress.emit("Forge tardó demasiado; se canceló la instalación y puedes volver a intentarlo.");
                }
                return Err(error);
            }
        };
        if !status.success() {
            for line in &recent_output {
                progress.emit(line);
            }
            return Err(format!("El instalador Forge terminó con código {:?}. Revisa los últimos mensajes del proceso.", status.code()));
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

fn ensure_forge_launcher_profiles(game_dir: &Path) -> Result<(), String> {
    let standard = game_dir.join("launcher_profiles.json");
    let microsoft_store = game_dir.join("launcher_profiles_microsoft_store.json");
    if standard.is_file() || microsoft_store.is_file() {
        return Ok(());
    }
    let mut file = match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&standard)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists && standard.is_file() => {
            return Ok(())
        }
        Err(error) => {
            return Err(format!(
                "No se pudo preparar el perfil de Minecraft que necesita Forge: {error}"
            ))
        }
    };
    file.write_all(br#"{"profiles":{},"settings":{},"version":3}"#)
        .and_then(|()| file.sync_all())
        .map_err(|error| {
            let _ = fs::remove_file(&standard);
            format!("No se pudo escribir el perfil de Minecraft para Forge: {error}")
        })
}

fn forward_forge_output<R: Read + Send + 'static>(
    stream: R,
    stream_name: &'static str,
    sender: mpsc::SyncSender<String>,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        let mut bytes = Vec::new();
        loop {
            bytes.clear();
            match read_bounded_forge_line(&mut reader, &mut bytes) {
                Ok(None) => break,
                Ok(Some(truncated)) => {
                    let mut line = String::from_utf8_lossy(&bytes).trim().to_string();
                    if line.is_empty() {
                        continue;
                    }
                    if truncated || line.chars().count() > 2048 {
                        line = line.chars().take(2048).collect();
                        line.push('…');
                    }
                    if sender.send(format!("[Forge {stream_name}] {line}")).is_err() {
                        break;
                    }
                }
                Err(error) => {
                    let _ = sender.send(format!("[Forge {stream_name}] Error leyendo salida: {error}"));
                    break;
                }
            }
        }
    })
}

const MAX_FORGE_OUTPUT_LINE_BYTES: usize = 8192;

fn read_bounded_forge_line<R: BufRead>(
    reader: &mut R,
    bytes: &mut Vec<u8>,
) -> io::Result<Option<bool>> {
    bytes.clear();
    let mut truncated = false;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok((!bytes.is_empty()).then_some(truncated));
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |index| index + 1);
        let remaining = MAX_FORGE_OUTPUT_LINE_BYTES.saturating_sub(bytes.len());
        let copied = consumed.min(remaining);
        bytes.extend_from_slice(&available[..copied]);
        truncated |= copied < consumed;
        reader.consume(consumed);
        if newline.is_some() {
            return Ok(Some(truncated));
        }
    }
}

#[cfg(test)]
mod forge_output_tests {
    use super::*;

    #[test]
    fn forwards_forge_output_lines_and_trims_line_endings() {
        let (sender, receiver) = mpsc::sync_channel(32);
        let reader = forward_forge_output(
            io::Cursor::new(b"Preparing libraries\r\nInstalled Forge\n".to_vec()),
            "stdout",
            sender,
        );

        let lines = receiver.into_iter().collect::<Vec<_>>();
        reader.join().unwrap();

        assert_eq!(
            lines,
            vec![
                "[Forge stdout] Preparing libraries",
                "[Forge stdout] Installed Forge"
            ]
        );
    }

    #[test]
    fn limits_a_single_forge_output_line() {
        let (sender, receiver) = mpsc::sync_channel(32);
        let reader = forward_forge_output(
            io::Cursor::new(format!("{}\n", "x".repeat(4096)).into_bytes()),
            "stderr",
            sender,
        );

        let line = receiver.into_iter().next().unwrap();
        reader.join().unwrap();

        assert_eq!(
            line.chars().count(),
            2048 + "[Forge stderr] ".chars().count() + 1
        );
        assert!(line.ends_with('…'));
    }

    #[test]
    fn bounds_forge_line_memory_and_keeps_following_lines_readable() {
        let input = format!("{}\nnext line\n", "x".repeat(MAX_FORGE_OUTPUT_LINE_BYTES + 1024));
        let mut reader = BufReader::new(io::Cursor::new(input.into_bytes()));
        let mut bytes = Vec::new();

        assert_eq!(read_bounded_forge_line(&mut reader, &mut bytes).unwrap(), Some(true));
        assert_eq!(bytes.len(), MAX_FORGE_OUTPUT_LINE_BYTES);
        assert_eq!(read_bounded_forge_line(&mut reader, &mut bytes).unwrap(), Some(false));
        assert_eq!(String::from_utf8(bytes).unwrap(), "next line\n");
    }
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

fn validate_personal_mod_size(size_bytes: u64, limit_bytes: u64) -> Result<(), String> {
    if size_bytes > limit_bytes {
        let megabyte = 1024 * 1024;
        let limit = if limit_bytes % megabyte == 0 {
            format!("{} MiB", limit_bytes / megabyte)
        } else {
            format!("{limit_bytes} bytes")
        };
        return Err(format!(
            "El mod personal supera el límite permitido de {limit}"
        ));
    }
    Ok(())
}

fn check_forge_mod_archive_directory(reader: &mut (impl Read + Seek)) -> Result<(), String> {
    const EOCD_SIGNATURE: &[u8; 4] = b"PK\x05\x06";
    const EOCD_FIXED_BYTES: usize = 22;
    const MAX_EOCD_TAIL_BYTES: u64 = EOCD_FIXED_BYTES as u64 + u16::MAX as u64;

    let file_len = reader
        .seek(SeekFrom::End(0))
        .map_err(|error| format!("No se pudo revisar el directorio ZIP: {error}"))?;
    let tail_start = file_len.saturating_sub(MAX_EOCD_TAIL_BYTES);
    reader
        .seek(SeekFrom::Start(tail_start))
        .map_err(|error| format!("No se pudo revisar el directorio ZIP: {error}"))?;
    let mut tail = Vec::with_capacity((file_len - tail_start) as usize);
    reader
        .read_to_end(&mut tail)
        .map_err(|error| format!("No se pudo revisar el directorio ZIP: {error}"))?;

    if tail.len() < EOCD_FIXED_BYTES {
        return Err("El JAR no contiene un directorio ZIP completo".into());
    }
    let eocd = (0..=tail.len() - EOCD_FIXED_BYTES)
        .rev()
        .find(|&offset| {
            tail.get(offset..offset + 4) == Some(EOCD_SIGNATURE.as_slice())
                && offset + EOCD_FIXED_BYTES
                    + u16::from_le_bytes([tail[offset + 20], tail[offset + 21]]) as usize
                    == tail.len()
        })
        .ok_or_else(|| "El JAR no contiene un cierre ZIP válido".to_string())?;

    let disk_number = u16::from_le_bytes([tail[eocd + 4], tail[eocd + 5]]);
    let directory_disk = u16::from_le_bytes([tail[eocd + 6], tail[eocd + 7]]);
    let disk_entries = u16::from_le_bytes([tail[eocd + 8], tail[eocd + 9]]);
    let total_entries = u16::from_le_bytes([tail[eocd + 10], tail[eocd + 11]]);
    let directory_bytes = u32::from_le_bytes([
        tail[eocd + 12],
        tail[eocd + 13],
        tail[eocd + 14],
        tail[eocd + 15],
    ]);

    if disk_number != 0 || directory_disk != 0 || disk_entries != total_entries {
        return Err("Los JAR multidisco no son compatibles".into());
    }
    if total_entries == u16::MAX || directory_bytes == u32::MAX {
        return Err("El JAR ZIP64 supera los límites seguros del validador".into());
    }
    if total_entries > MAX_FORGE_MOD_ARCHIVE_ENTRIES {
        return Err(format!(
            "El JAR contiene más de {MAX_FORGE_MOD_ARCHIVE_ENTRIES} entradas"
        ));
    }
    if directory_bytes > MAX_FORGE_MOD_CENTRAL_DIRECTORY_BYTES {
        return Err("El directorio central del JAR supera los 32 MiB permitidos".into());
    }
    Ok(())
}

fn validate_forge_mod_archive(path: &Path) -> Result<(), String> {
    let mut file = fs::File::open(path)
        .map_err(|error| format!("No se pudo abrir el JAR {}: {error}", path.display()))?;
    check_forge_mod_archive_directory(&mut file)?;
    file.seek(SeekFrom::Start(0))
        .map_err(|error| format!("No se pudo revisar el JAR {}: {error}", path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|error| format!("El archivo seleccionado no es un JAR válido: {error}"))?;
    let metadata_index = {
        archive
            .file_names()
            .position(|name| name.eq_ignore_ascii_case("META-INF/mods.toml"))
    };
    if let Some(metadata_index) = metadata_index {
        let mut metadata = archive
            .by_index(metadata_index)
            .map_err(|error| format!("No se pudo leer mods.toml del JAR: {error}"))?;
        if metadata.size() > MAX_FORGE_MOD_METADATA_BYTES {
            return Err("mods.toml del JAR supera el tamaño máximo permitido".into());
        }
        let mut contents = String::new();
        metadata
            .by_ref()
            .take(MAX_FORGE_MOD_METADATA_BYTES)
            .read_to_string(&mut contents)
            .map_err(|error| format!("mods.toml del JAR no es texto UTF-8 válido: {error}"))?;
        let document: toml::Value = toml::from_str(&contents)
            .map_err(|error| format!("mods.toml del JAR no es TOML válido: {error}"))?;
        for required in ["modLoader", "loaderVersion"] {
            if document
                .get(required)
                .and_then(toml::Value::as_str)
                .is_none_or(|value| value.trim().is_empty())
            {
                return Err(format!("mods.toml no declara {required} válido"));
            }
        }
        let mods = document
            .get("mods")
            .and_then(toml::Value::as_array)
            .filter(|mods| !mods.is_empty())
            .ok_or_else(|| "mods.toml no declara ningún mod Forge".to_string())?;
        let mut mod_ids = BTreeSet::new();
        for entry in mods {
            let mod_id = entry
                .get("modId")
                .and_then(toml::Value::as_str)
                .filter(|value| valid_forge_mod_id(value))
                .ok_or_else(|| "mods.toml contiene un modId ausente o no válido".to_string())?;
            if !mod_ids.insert(mod_id) {
                return Err(format!("mods.toml repite el modId {mod_id}"));
            }
        }
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

fn valid_forge_mod_id(value: &str) -> bool {
    let mut bytes = value.bytes();
    matches!(bytes.next(), Some(b'a'..=b'z'))
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        && (2..=64).contains(&value.len())
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

fn copy_file_create_new_limited(
    source: &Path,
    destination: &Path,
    max_bytes: u64,
) -> io::Result<u64> {
    let mut input = fs::File::open(source)?;
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    let result = (|| {
        let copied = io::copy(&mut Read::by_ref(&mut input).take(max_bytes.saturating_add(1)), &mut output)?;
        if copied > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "source exceeded the configured size limit",
            ));
        }
        output.sync_all()?;
        Ok(copied)
    })();
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

fn ensure_series_not_running(app: &AppHandle, series_id: &str) -> Result<(), String> {
    let status = get_minecraft_status_sync(app.clone())?;
    ensure_series_status_not_running(&status, series_id)?;
    if let Some(game_directory) = resolve_isolated_series_game_directory(app, series_id)? {
        ensure_game_directory_not_running(&game_directory)?;
    }
    Ok(())
}

fn ensure_game_directory_not_running(game_directory: &Path) -> Result<(), String> {
    if let Some(pid) = find_running_minecraft_pid(game_directory) {
        return Err(format!(
            "Minecraft de esta instancia (PID {pid}) sigue ejecutándose. Ciérralo antes de cambiar sus mods o actualizar el pack."
        ));
    }
    Ok(())
}

fn java_process_name(name: &OsStr) -> bool {
    matches!(
        name.to_string_lossy().to_ascii_lowercase().as_str(),
        "java" | "java.exe" | "javaw" | "javaw.exe"
    )
}

fn hide_console_if_windows(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    #[cfg(not(windows))]
    let _ = command;
}

fn game_directory_argument(args: &[OsString]) -> Option<PathBuf> {
    for (index, arg) in args.iter().enumerate() {
        if arg == OsStr::new("--gameDir") {
            return args.get(index + 1).map(PathBuf::from);
        }
        if let Some(value) = arg.to_string_lossy().strip_prefix("--gameDir=") {
            return Some(PathBuf::from(value));
        }
    }
    None
}

fn normalized_process_path(path: &Path, working_directory: Option<&Path>) -> Option<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        working_directory?.join(path)
    };
    Some(absolute.canonicalize().unwrap_or(absolute))
}

fn paths_equal_for_process_check(left: &Path, right: &Path) -> bool {
    #[cfg(windows)]
    {
        left.to_string_lossy()
            .replace('/', "\\")
            .eq_ignore_ascii_case(&right.to_string_lossy().replace('/', "\\"))
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

fn process_minecraft_game_directory(process: &sysinfo::Process) -> Option<PathBuf> {
    if !java_process_name(process.name())
        && !process
            .exe()
            .and_then(Path::file_name)
            .is_some_and(java_process_name)
    {
        return None;
    }
    game_directory_argument(process.cmd())
        .and_then(|path| normalized_process_path(&path, process.cwd()))
}

fn running_minecraft_directories() -> Vec<(u32, PathBuf)> {
    let mut system = System::new();
    // First read only process names. Command lines can contain unrelated tokens,
    // so fetch them only for Java candidates whose arguments need inspection.
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().without_tasks(),
    );
    let java_pids = system
        .processes()
        .iter()
        .filter(|(_, process)| {
            java_process_name(process.name())
                || process
                    .exe()
                    .and_then(Path::file_name)
                    .is_some_and(java_process_name)
        })
        .map(|(pid, _)| *pid)
        .collect::<Vec<_>>();
    if java_pids.is_empty() {
        return Vec::new();
    }
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&java_pids),
        true,
        ProcessRefreshKind::nothing()
            .without_tasks()
            .with_cmd(UpdateKind::Always)
            .with_cwd(UpdateKind::Always),
    );
    java_pids
        .into_iter()
        .filter_map(|pid| {
            let process = system.process(pid)?;
            let directory = process_minecraft_game_directory(process)?;
            Some((pid.as_u32(), directory))
        })
        .collect()
}

fn find_running_minecraft_pid(game_directory: &Path) -> Option<u32> {
    let expected_directory = game_directory
        .canonicalize()
        .unwrap_or_else(|_| game_directory.to_path_buf());
    running_minecraft_directories()
        .into_iter()
        .find(|(_, directory)| paths_equal_for_process_check(directory, &expected_directory))
        .map(|(pid, _)| pid)
}

fn detect_external_minecraft(app: &AppHandle) -> Result<Option<MinecraftStatus>, String> {
    let mut directories = Vec::new();
    for series in current_catalog()?.series {
        if let Some(game_directory) = resolve_isolated_series_game_directory(app, &series.id)? {
            directories.push((series.id, game_directory));
        }
    }
    if directories.is_empty() {
        return Ok(None);
    }
    let processes = running_minecraft_directories();
    for (series_id, game_directory) in directories {
        let expected_directory = game_directory
            .canonicalize()
            .unwrap_or(game_directory);
        if let Some((pid, _)) = processes
            .iter()
            .find(|(_, directory)| paths_equal_for_process_check(directory, &expected_directory))
        {
            return Ok(Some(MinecraftStatus {
                running: true,
                series_id: Some(series_id),
                pid: Some(*pid),
                exit_code: None,
                exit_success: None,
            }));
        }
    }
    Ok(None)
}

fn ensure_series_status_not_running(status: &MinecraftStatus, series_id: &str) -> Result<(), String> {
    if status.running {
        match status.series_id.as_deref() {
            Some(running_series) if running_series != series_id => return Ok(()),
            Some(_) => {
                return Err(
                    "Cierra Minecraft de esta serie antes de cambiar sus mods o actualizar el pack."
                        .into(),
                );
            }
            None => {
                return Err(
                    "No se pudo identificar la instancia en ejecución; cierra Minecraft antes de modificar packs o mods."
                        .into(),
                );
            }
        }
    }
    Ok(())
}

fn add_personal_mod(source: &Path, mods_directory: &Path) -> Result<String, String> {
    let lock_root = mods_directory.parent().unwrap_or(mods_directory);
    let _guard = pack::lock_pack_sync(lock_root)?;
    add_personal_mod_inner(source, mods_directory)
}

fn add_personal_mod_inner(source: &Path, mods_directory: &Path) -> Result<String, String> {
    add_personal_mod_inner_with_limit(source, mods_directory, pack::MOD_LIMIT)
}

fn add_personal_mod_inner_with_limit(
    source: &Path,
    mods_directory: &Path,
    max_bytes: u64,
) -> Result<String, String> {
    let source = source
        .canonicalize()
        .map_err(|error| format!("No se pudo resolver el archivo seleccionado: {error}"))?;
    let metadata = fs::metadata(&source)
        .map_err(|error| format!("No se pudo leer el archivo seleccionado: {error}"))?;
    if !metadata.is_file() {
        return Err("Selecciona un archivo JAR normal".into());
    }
    validate_personal_mod_size(metadata.len(), max_bytes)?;
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
    if let Err(error) = copy_file_create_new_limited(&source, &temporary, max_bytes) {
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
    let lock_root = mods_directory.parent().unwrap_or(mods_directory);
    let _guard = pack::lock_pack_sync(lock_root)?;
    remove_personal_mod_inner(name, mods_directory)
}

fn remove_personal_mod_inner(name: &str, mods_directory: &Path) -> Result<(), String> {
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
    let lock_root = mods_directory.parent().unwrap_or(mods_directory);
    let _guard = pack::lock_pack_sync(lock_root)?;
    activate_personal_mod_inner(name, mods_directory)
}

fn activate_personal_mod_inner(name: &str, mods_directory: &Path) -> Result<(), String> {
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

const JAVA_VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_JAVA_VERSION_OUTPUT_BYTES: usize = 16 * 1024;

fn read_java_version_pipe<R: Read + Send + 'static>(
    reader: R,
) -> (mpsc::Receiver<io::Result<Vec<u8>>>, thread::JoinHandle<()>) {
    let (sender, receiver) = mpsc::channel();
    let handle = thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = reader
            .take((MAX_JAVA_VERSION_OUTPUT_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .and_then(|_| {
                if bytes.len() > MAX_JAVA_VERSION_OUTPUT_BYTES {
                    Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Java devolvió más datos de versión de los permitidos",
                    ))
                } else {
                    Ok(bytes)
                }
            });
        let _ = sender.send(result);
    });
    (receiver, handle)
}

fn finish_java_version_pipe(
    receiver: mpsc::Receiver<io::Result<Vec<u8>>>,
    reader: thread::JoinHandle<()>,
    timeout: Duration,
) -> Result<Vec<u8>, String> {
    let result = receiver.recv_timeout(timeout).map_err(|error| match error {
        mpsc::RecvTimeoutError::Timeout => {
            "Java dejó abierta su salida después de terminar la comprobación".to_string()
        }
        mpsc::RecvTimeoutError::Disconnected => {
            "Falló la lectura de la salida de Java".to_string()
        }
    })?;
    reader
        .join()
        .map_err(|_| "Falló la lectura de la salida de Java".to_string())?;
    result.map_err(|error| format!("No se pudo leer la salida de Java: {error}"))
}

#[cfg(unix)]
fn terminate_java_probe_group(process_id: u32) {
    // The probe is placed in its own process group before spawn. If a wrapper
    // exits while a child still owns stdout/stderr, kill that orphaned child
    // so the pipe readers can finish as well.
    unsafe {
        libc::kill(-(process_id as libc::pid_t), libc::SIGKILL);
    }
}

fn java_version_output(path: &Path, timeout: Duration) -> Result<std::process::Output, String> {
    let mut command = Command::new(path);
    hide_console_if_windows(&mut command);
    command
        .arg("-version")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .map_err(|error| format!("No se pudo ejecutar Java: {error}"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "No se pudo capturar la salida de Java".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "No se pudo capturar los errores de Java".to_string())?;
    let (stdout_receiver, stdout_reader) = read_java_version_pipe(stdout);
    let (stderr_receiver, stderr_reader) = read_java_version_pipe(stderr);
    let deadline = Instant::now() + timeout;

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stdout = finish_java_version_pipe(
                    stdout_receiver,
                    stdout_reader,
                    deadline.saturating_duration_since(Instant::now()),
                );
                let stderr = finish_java_version_pipe(
                    stderr_receiver,
                    stderr_reader,
                    deadline.saturating_duration_since(Instant::now()),
                );
                if stdout.is_err() || stderr.is_err() {
                    #[cfg(unix)]
                    terminate_java_probe_group(child.id());
                }
                return Ok(std::process::Output {
                    status,
                    stdout: stdout?,
                    stderr: stderr?,
                });
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(25)),
            Ok(None) => {
                #[cfg(unix)]
                terminate_java_probe_group(child.id());
                let _ = child.kill();
                let _ = child.wait();
                let _ = finish_java_version_pipe(
                    stdout_receiver,
                    stdout_reader,
                    Duration::from_millis(100),
                );
                let _ = finish_java_version_pipe(
                    stderr_receiver,
                    stderr_reader,
                    Duration::from_millis(100),
                );
                return Err(format!(
                    "La comprobación de Java superó el límite de {} segundos",
                    timeout.as_secs_f32()
                ));
            }
            Err(error) => {
                #[cfg(unix)]
                terminate_java_probe_group(child.id());
                let _ = child.kill();
                let _ = child.wait();
                let _ = finish_java_version_pipe(
                    stdout_receiver,
                    stdout_reader,
                    Duration::from_millis(100),
                );
                let _ = finish_java_version_pipe(
                    stderr_receiver,
                    stderr_reader,
                    Duration::from_millis(100),
                );
                return Err(format!("No se pudo consultar el proceso Java: {error}"));
            }
        }
    }
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
    #[cfg(windows)]
    candidates.extend(windows_java_candidates());
    #[cfg(target_os = "linux")]
    for root in ["/usr/lib/jvm", "/usr/java"] {
        if let Ok(entries) = fs::read_dir(root) {
            candidates.extend(entries.flatten().map(|entry| entry.path().join("bin/java")));
        }
    }
    candidates
}

#[cfg(any(windows, test))]
fn append_java_candidates_under_install_root(root: &Path, candidates: &mut Vec<PathBuf>) {
    candidates.push(root.join("java.exe"));
    candidates.push(root.join("bin").join("java.exe"));
    candidates.push(root.join("javapath").join("java.exe"));

    if let Ok(entries) = fs::read_dir(root) {
        for entry in entries.flatten() {
            let installation = entry.path();
            if installation.is_dir() {
                candidates.push(installation.join("bin").join("java.exe"));
                candidates.push(installation.join("jre").join("bin").join("java.exe"));
            }
        }
    }
}

#[cfg(windows)]
fn windows_java_candidates() -> Vec<PathBuf> {
    let program_files_roots = ["ProgramFiles", "ProgramFiles(x86)"]
        .into_iter()
        .filter_map(env::var_os)
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    let local_app_data = env::var_os("LOCALAPPDATA").map(PathBuf::from);
    windows_java_candidates_from_roots(&program_files_roots, local_app_data.as_deref())
}

#[cfg(any(windows, test))]
fn windows_java_candidates_from_roots(
    program_files_roots: &[PathBuf],
    local_app_data: Option<&Path>,
) -> Vec<PathBuf> {
    let mut installation_roots = Vec::new();
    for program_files in program_files_roots {
        installation_roots.push(program_files.join("Common Files/Oracle/Java/javapath"));
        for vendor in [
            "Java",
            "Eclipse Adoptium",
            "Microsoft",
            "Microsoft JDK",
            "Amazon Corretto",
            "Azul Systems",
            "Azul Systems/Zulu",
            "Zulu",
            "GraalVM",
            "IBM/Semeru",
            "Semeru",
            "BellSoft",
        ] {
            installation_roots.push(program_files.join(vendor));
        }
    }
    if let Some(local_app_data) = local_app_data {
        installation_roots.push(local_app_data.join("Programs/Java"));
        installation_roots.push(local_app_data.join("Programs/Eclipse Adoptium"));
        installation_roots.push(local_app_data.join("Programs/Microsoft"));
        installation_roots.push(local_app_data.join("Microsoft/WinGet/Links"));
    }

    let mut candidates = Vec::new();
    for root in installation_roots {
        append_java_candidates_under_install_root(&root, &mut candidates);
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
        match java_version_output(&candidate, JAVA_VERSION_PROBE_TIMEOUT) {
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
    match java_version_output(path, JAVA_VERSION_PROBE_TIMEOUT) {
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

fn ensure_java_executable(_path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        let metadata = fs::metadata(_path)
            .map_err(|error| format!("No se pudo inspeccionar el runtime Java: {error}"))?;
        let mode = metadata.permissions().mode();
        if mode & 0o111 == 0 {
            fs::set_permissions(_path, fs::Permissions::from_mode(mode | 0o111))
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

fn update_supported_for(target_os: &str, is_appimage: bool) -> bool {
    target_os == "windows" || (target_os == "linux" && is_appimage)
}

#[tauri::command]
fn launcher_updater_supported() -> bool {
    update_supported_for(
        std::env::consts::OS,
        std::env::var_os("APPIMAGE").is_some(),
    )
}

const LAUNCHER_RELEASES_URL: &str = "https://github.com/Santi-PdR/EternalCraft-Launcher/releases/latest";

#[tauri::command]
fn open_launcher_releases() -> Result<(), String> {
    open_system_browser(LAUNCHER_RELEASES_URL)
        .map_err(|error| format!("No se pudo abrir las descargas oficiales: {error}"))
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

#[cfg(any(target_os = "linux", test))]
fn effective_memory_mb(host_total_mb: u32, cgroup_limit_bytes: Option<u64>) -> u32 {
    let Some(limit_bytes) = cgroup_limit_bytes else {
        return host_total_mb;
    };
    let limit_mb = (limit_bytes / (1024 * 1024)).min(u32::MAX as u64) as u32;
    if limit_mb == 0 {
        host_total_mb
    } else {
        host_total_mb.min(limit_mb)
    }
}

#[cfg(any(target_os = "linux", test))]
fn cgroup_memory_limit_in_tree(
    mount_root: &Path,
    group_path: &str,
    limit_file: &str,
) -> Option<u64> {
    let mut limit_components = Path::new(limit_file).components();
    if !matches!(
        limit_components.next(),
        Some(std::path::Component::Normal(_))
    ) || limit_components.next().is_some()
    {
        return None;
    }
    let relative = Path::new(group_path).strip_prefix("/").ok()?;
    if relative
        .components()
        .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return None;
    }
    let mut directory = mount_root.join(relative);
    if !directory.starts_with(mount_root) {
        return None;
    }

    let mut most_restrictive = None;
    loop {
        if let Ok(content) = fs::read_to_string(directory.join(limit_file)) {
            if let Ok(limit) = content.trim().parse::<u64>() {
                most_restrictive = Some(most_restrictive.map_or(limit, |current: u64| current.min(limit)));
            }
        }
        if directory == mount_root || !directory.pop() || !directory.starts_with(mount_root) {
            break;
        }
    }
    most_restrictive
}

#[cfg(any(target_os = "linux", test))]
fn cgroup_memory_limit_from_membership(
    membership: &str,
    v2_mount: &Path,
    v1_memory_mount: &Path,
) -> Option<u64> {
    let mut most_restrictive = None;
    for line in membership.lines() {
        let mut fields = line.splitn(3, ':');
        let Some(hierarchy) = fields.next() else { continue };
        let Some(controllers) = fields.next() else { continue };
        let Some(group_path) = fields.next() else { continue };
        let limit = if hierarchy == "0" && controllers.is_empty() {
            cgroup_memory_limit_in_tree(v2_mount, group_path, "memory.max")
        } else if controllers.split(',').any(|controller| controller == "memory") {
            cgroup_memory_limit_in_tree(v1_memory_mount, group_path, "memory.limit_in_bytes")
        } else {
            None
        };
        if let Some(limit) = limit {
            most_restrictive = Some(most_restrictive.map_or(limit, |current: u64| current.min(limit)));
        }
    }
    most_restrictive
}

#[cfg(target_os = "linux")]
fn linux_cgroup_memory_limit_bytes() -> Option<u64> {
    let membership = fs::read_to_string("/proc/self/cgroup").ok()?;
    cgroup_memory_limit_from_membership(
        &membership,
        Path::new("/sys/fs/cgroup"),
        Path::new("/sys/fs/cgroup/memory"),
    )
}

fn memory_status(settings: &Settings) -> MemoryStatus {
    static TOTAL_MEMORY_MB: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    let total_mb = *TOTAL_MEMORY_MB.get_or_init(|| {
        let mut system = System::new();
        system.refresh_memory();
        let host_total_mb = (system.total_memory() / (1024 * 1024)).min(u32::MAX as u64) as u32;
        #[cfg(target_os = "linux")]
        let total_mb = effective_memory_mb(host_total_mb, linux_cgroup_memory_limit_bytes());
        #[cfg(not(target_os = "linux"))]
        let total_mb = host_total_mb;
        total_mb
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
    let settings = update_settings(app, |settings| {
        let mut migrated = migrate_theme_preference(settings);
        migrated |= migrate_microsoft_client_id_preference(settings);
        Ok(migrated)
    })?;
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
        .app_local_data_dir()
        .map_err(|error| error.to_string())?;
    let roaming_app_data_dir = app
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
    let session_state = app.state::<MicrosoftSessionStatus>();
    let (session_restore_complete, microsoft_session_notice) = {
        let session_status = session_state
            .0
            .lock()
            .map_err(|_| "El estado de la sesión Microsoft quedó bloqueado".to_string())?;
        (session_status.restore_attempted, session_status.notice.clone())
    };
    let java_manually_selected = settings.java_executable.is_some();
    let java = if let Some(path) = settings.java_executable.as_deref() {
        inspect_java(Path::new(path))
    } else {
        let detected = detect_java();
        if detected.compatible {
            detected
        } else {
            let active_dir =
                resolve_managed_instance_dir(&app_data_dir, &roaming_app_data_dir, &active)?;
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
        let game_dir =
            resolve_managed_instance_dir(&app_data_dir, &roaming_app_data_dir, &series.id)?;
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
        theme_id: resolve_theme(settings.theme_id),
        background_preset: settings.background_preset,
        java,
        java_manually_selected,
        managed_game_directories,
        installed_profiles,
        microsoft_login_available: configured_microsoft_client_id().is_some(),
        session_restore_complete,
        microsoft_session_notice,
        github_developer_enabled: developer::github_app_client_id_configured(),
        developer_github_user: app.state::<developer::GitHubDeveloper>().username(),
        microsoft_profile,
        offline_username: settings.offline_username,
        account_mode: settings.account_mode,
        memory,
    })
}

#[tauri::command]
fn set_theme(app: AppHandle, theme_id: String) -> Result<Bootstrap, String> {
    if !is_valid_theme(&theme_id) {
        return Err("El tema solicitado no está disponible".into());
    }
    update_settings(&app, |settings| {
        settings.theme_id = Some(theme_id);
        settings.theme_migrated = true;
        Ok(true)
    })?;
    make_bootstrap(&app)
}

fn is_valid_background_preset(preset_id: &str) -> bool {
    matches!(preset_id, "siege" | "ghouls-outbreak")
}

#[tauri::command]
fn set_background_preset(app: AppHandle, preset_id: String) -> Result<Bootstrap, String> {
    if preset_id != "auto" && !is_valid_background_preset(&preset_id) {
        return Err("El fondo seleccionado no está disponible".into());
    }
    update_settings(&app, |settings| {
        settings.background_preset = Some(preset_id);
        Ok(true)
    })?;
    make_bootstrap(&app)
}

#[tauri::command]
async fn get_bootstrap(app: AppHandle) -> Result<Bootstrap, String> {
    let cache_path = app
        .path()
        .app_config_dir()
        .map_err(|error| error.to_string())?
        .join("series-catalog.json");
    let initial_cache_path = cache_path.clone();
    tauri::async_runtime::spawn_blocking(move || {
        load_runtime_catalog_cache(&initial_cache_path)
    })
        .await
        .map_err(|error| format!("Falló la lectura de la caché del catálogo: {error}"))??;
    let bootstrap = make_bootstrap(&app)?;

    let restore_app = app.clone();
    let _session_restore_task = tauri::async_runtime::spawn_blocking(move || {
        if restore_microsoft_session_once(&restore_app).unwrap_or(false) {
            if let Ok(updated) = make_bootstrap(&restore_app) {
                let _ = restore_app.emit("session-updated", updated);
            }
        }
    });

    let refresh_app = app.clone();
    let _refresh_task = tauri::async_runtime::spawn_blocking(move || {
        if refresh_runtime_catalog(&cache_path).is_ok() {
            if let Ok(updated) = make_bootstrap(&refresh_app) {
                let _ = refresh_app.emit("catalog-updated", updated);
            }
        }
    });
    Ok(bootstrap)
}

#[tauri::command]
fn get_launcher_logs(app: AppHandle, series_id: String) -> Result<String, String> {
    let path = app
        .path()
        .app_local_data_dir()
        .map_err(|error| error.to_string())?
        .join("logs")
        .join("launcher.log");
    let launcher_log = if path.exists() {
        read_install_log_at(&path)?
    } else {
        let legacy_path = app
            .path()
            .app_data_dir()
            .map_err(|error| error.to_string())?
            .join("logs")
            .join("launcher.log");
        read_install_log_at(&legacy_path)?
    };
    let mut sections = vec![format!("=== EternalCraft Launcher ===\n{launcher_log}")];
    if let Some(game_directory) = resolve_series_game_directory(&app, &series_id)? {
        let game_log_path = game_directory.join("logs").join("latest.log");
        if let Some(game_log) = read_log_tail(&game_log_path, 256 * 1024)? {
            sections.push(format!("=== Minecraft · {series_id} ===\n{game_log}"));
        }
        if let Some((name, report)) = read_latest_crash_report(&game_directory)? {
            sections.push(format!("=== Informe de cierre · {name} ===\n{report}"));
        }
        if let Some((name, report)) = read_latest_jvm_fatal_log(&game_directory)? {
            sections.push(format!("=== Error fatal de Java · {name} ===\n{report}"));
        }
    }
    Ok(sections.join("\n\n"))
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
    let progress_app = app.clone();
    let guard_app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let lock_root = guard_app.path().app_data_dir().map_err(|error| error.to_string())?;
        let _guard = pack::lock_pack_sync(&lock_root)?;
        ensure_series_not_running(&guard_app, &series_id)?;
        migrate_managed_instance_for_series(&guard_app, &series_id)?;
        let game_dir = resolve_isolated_series_game_directory(&guard_app, &series_id)?
            .ok_or_else(|| "Primero instala o vincula una carpeta de juego para esta serie".to_string())?;
        verify_forge_profile(&game_dir, &series)?;
        pack::sync_pack_locked(
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

fn configured_microsoft_client_id() -> Option<&'static str> {
    option_env!("ETERNALCRAFT_MICROSOFT_CLIENT_ID")
        .map(str::trim)
        .filter(|id| is_valid_microsoft_client_id(id))
}

fn bind_microsoft_callback_listeners(
) -> Result<(std::net::TcpListener, Option<std::net::TcpListener>), String> {
    let ipv4 = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .map_err(|error| format!("No se pudo abrir el callback local de inicio de sesión: {error}"))?;
    ipv4.set_nonblocking(true)
        .map_err(|error| format!("No se pudo preparar el callback local: {error}"))?;
    let port = ipv4
        .local_addr()
        .map_err(|error| format!("No se pudo identificar el puerto local: {error}"))?
        .port();
    // localhost may resolve to either address family. Keep both listeners bound
    // to loopback only so browser address preference cannot strand the callback.
    let ipv6 = std::net::TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, port))
        .ok()
        .and_then(|listener| listener.set_nonblocking(true).ok().map(|()| listener));
    Ok((ipv4, ipv6))
}

fn microsoft_oauth_callback_error(
    callback: &reqwest::Url,
    expected_state: &str,
) -> Option<&'static str> {
    let mut callback_state = None;
    let mut provider_error = None;
    for (key, value) in callback.query_pairs() {
        match key.as_ref() {
            "state" if callback_state.is_none() => callback_state = Some(value.into_owned()),
            "error" if provider_error.is_none() => provider_error = Some(value.into_owned()),
            "state" | "error" => return None,
            _ => {}
        }
    }
    if callback_state.as_deref() != Some(expected_state) {
        return None;
    }
    match provider_error.as_deref()? {
        "access_denied" => Some("Inicio de sesión Microsoft cancelado o denegado. Puedes intentarlo de nuevo."),
        _ => Some("Microsoft no pudo completar el inicio de sesión. Inténtalo de nuevo."),
    }
}

fn accept_microsoft_callback(
    listener: &std::net::TcpListener,
    redirect_uri: &str,
    expected_state: &str,
) -> Result<Option<String>, String> {
    let (mut stream, peer) = match listener.accept() {
        Ok(connection) => connection,
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(None),
        Err(error) => return Err(format!("Falló el callback local de Microsoft: {error}")),
    };
    if !peer.ip().is_loopback() {
        return Ok(None);
    }
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .map_err(|error| format!("No se pudo limitar la espera del callback: {error}"))?;
    let mut request = [0_u8; 8192];
    let read = match stream.read(&mut request) {
        Ok(read) => read,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
            ) =>
        {
            return Ok(None)
        }
        Err(error) => return Err(format!("No se pudo leer el callback de Microsoft: {error}")),
    };
    let request = String::from_utf8_lossy(&request[..read]);
    let first_line = request.lines().next().unwrap_or_default();
    let mut parts = first_line.split_ascii_whitespace();
    if parts.next() != Some("GET") {
        let _ = stream.write_all(b"HTTP/1.1 405 Method Not Allowed\r\nConnection: close\r\n\r\n");
        return Ok(None);
    }
    let Some(target) = parts.next().filter(|target| target.starts_with('/')) else {
        let _ = stream.write_all(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n");
        return Ok(None);
    };
    let callback = format!("{redirect_uri}{target}");
    let auth_code = match parse_auth_code_url(&callback, Some(expected_state.to_string())) {
        Ok(code) => code,
        Err(_) => {
            let oauth_error = reqwest::Url::parse(&callback)
                .ok()
                .and_then(|url| microsoft_oauth_callback_error(&url, expected_state));
            if let Some(message) = oauth_error {
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\n\r\nInicio de sesion cancelado. Puedes cerrar esta pestana.");
                return Err(message.to_string());
            }
            let _ = stream.write_all(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\nNo se pudo validar el inicio de sesion.");
            return Ok(None);
        }
    };
    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nConnection: close\r\n\r\nInicio de sesion recibido. Puedes volver a EternalCraft Launcher.");
    Ok(Some(auth_code))
}

#[tauri::command]
async fn login_microsoft(app: AppHandle) -> Result<Bootstrap, String> {
    tauri::async_runtime::spawn_blocking(move || login_microsoft_sync(app))
        .await
        .map_err(|error| format!("Falló la tarea de inicio Microsoft: {error}"))?
}

fn login_microsoft_sync(app: AppHandle) -> Result<Bootstrap, String> {
    let client_id = configured_microsoft_client_id()
        .ok_or_else(|| "El inicio de sesión Microsoft no está habilitado en esta compilación.".to_string())?;
    let (listener, ipv6_listener) = bind_microsoft_callback_listeners()?;
    let port = listener.local_addr().map_err(|error| error.to_string())?.port();
    let redirect_uri = format!("http://localhost:{port}");
    let (login_url, expected_state, verifier) =
        get_secure_login_data(client_id, &redirect_uri, None);
    open_system_browser(&login_url)
        .map_err(|error| format!("No se pudo abrir el navegador para iniciar sesión: {error}"))?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(180);
    let auth_code = loop {
        if std::time::Instant::now() >= deadline {
            return Err("Se agotó el tiempo esperando el regreso del navegador. Intenta iniciar sesión otra vez.".into());
        }
        let mut auth_code = None;
        for callback_listener in std::iter::once(&listener).chain(ipv6_listener.iter()) {
            if let Some(code) = accept_microsoft_callback(
                callback_listener,
                &redirect_uri,
                &expected_state,
            )? {
                auth_code = Some(code);
                break;
            }
        }
        if let Some(code) = auth_code {
            break code;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
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
    let persistence_notice = save_microsoft_refresh_token(&session.refresh_token)
        .err()
        .map(|_| "Sesión iniciada, pero el sistema no pudo guardar la credencial; tendrás que iniciar sesión otra vez al reiniciar.".to_string());
    if let Err(error) = update_settings(&app, |settings| {
        settings.account_mode = Some(AccountMode::Microsoft);
        Ok(true)
    }) {
        if persistence_notice.is_none() {
            let _ = delete_microsoft_refresh_token();
        }
        return Err(error);
    }
    *app.state::<AuthSession>()
        .0
        .lock()
        .map_err(|_| "La sesión Microsoft quedó bloqueada".to_string())? = Some(session);
    set_microsoft_restore_status(&app, persistence_notice)?;
    make_bootstrap(&app)
}

#[tauri::command]
fn logout_microsoft(app: AppHandle) -> Result<Bootstrap, String> {
    delete_microsoft_refresh_token()?;
    *app.state::<AuthSession>()
        .0
        .lock()
        .map_err(|_| "La sesión Microsoft quedó bloqueada".to_string())? = None;
    update_settings(&app, |settings| {
        if settings.account_mode == Some(AccountMode::Microsoft) {
            settings.account_mode = None;
            Ok(true)
        } else {
            Ok(false)
        }
    })?;
    set_microsoft_restore_status(&app, None)?;
    make_bootstrap(&app)
}

fn valid_offline_username(username: &str) -> bool {
    (3..=16).contains(&username.len())
        && username
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn valid_offline_uuid(uuid: &str) -> bool {
    let compact = uuid.replace('-', "");
    compact.len() == 32 && compact.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn offline_uuid_for_profile(
    username: &str,
    saved_username: Option<&str>,
    saved_uuid: Option<&str>,
) -> String {
    if saved_username == Some(username) {
        if let Some(uuid) = saved_uuid.filter(|uuid| valid_offline_uuid(uuid)) {
            return uuid.to_string();
        }
    }
    Account::offline(username).uuid().to_string()
}

#[tauri::command]
fn login_offline(app: AppHandle, username: String) -> Result<Bootstrap, String> {
    let username = username.trim();
    if !valid_offline_username(username) {
        return Err("El nombre local debe tener entre 3 y 16 caracteres: letras ASCII, números o guion bajo".into());
    }
    let credential_cleanup_notice = delete_microsoft_refresh_token()
        .err()
        .map(|_| "Perfil offline activado; la sesión Microsoft guardada no pudo eliminarse del llavero, pero no se restaurará mientras uses el perfil offline.".to_string());
    update_settings(&app, |settings| {
        let offline_uuid = offline_uuid_for_profile(
            username,
            settings.offline_username.as_deref(),
            settings.offline_uuid.as_deref(),
        );
        settings.offline_username = Some(username.to_string());
        settings.offline_uuid = Some(offline_uuid);
        settings.account_mode = Some(AccountMode::Offline);
        Ok(true)
    })?;
    *app.state::<AuthSession>()
        .0
        .lock()
        .map_err(|_| "La sesión Microsoft quedó bloqueada".to_string())? = None;
    set_microsoft_restore_status(&app, credential_cleanup_notice)?;
    make_bootstrap(&app)
}

#[tauri::command]
fn logout_offline(app: AppHandle) -> Result<Bootstrap, String> {
    update_settings(&app, |settings| {
        settings.offline_username = None;
        settings.offline_uuid = None;
        if settings.account_mode == Some(AccountMode::Offline) {
            settings.account_mode = None;
        }
        Ok(true)
    })?;
    make_bootstrap(&app)
}

#[tauri::command]
fn set_memory_limit(app: AppHandle, memory_mb: u32) -> Result<Bootstrap, String> {
    update_settings(&app, |settings| {
        let limits = memory_status(settings);
        if memory_mb < limits.min_mb || memory_mb > limits.max_mb || memory_mb % 512 != 0 {
            return Err(format!(
                "El valor debe estar entre {} y {} MiB en pasos de 512 MiB",
                limits.min_mb, limits.max_mb
            ));
        }
        settings.memory_limit_mb = Some(memory_mb);
        Ok(true)
    })?;
    make_bootstrap(&app)
}

#[tauri::command]
fn reset_memory_limit(app: AppHandle) -> Result<Bootstrap, String> {
    update_settings(&app, |settings| {
        settings.memory_limit_mb = None;
        Ok(true)
    })?;
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
async fn get_minecraft_status(app: AppHandle) -> Result<MinecraftStatus, String> {
    tauri::async_runtime::spawn_blocking(move || get_minecraft_status_sync(app))
        .await
        .map_err(|error| format!("Falló la consulta del estado de Minecraft: {error}"))?
}

fn get_minecraft_status_sync(app: AppHandle) -> Result<MinecraftStatus, String> {
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
        None => {
            if let Some(external) = detect_external_minecraft(&app)? {
                state.last_status = external.clone();
                return Ok(external);
            }
            if state.last_status.running {
                state.last_status.running = false;
                state.last_status.exit_code = None;
                state.last_status.exit_success = None;
            }
            return Ok(state.last_status.clone());
        }
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
async fn launch_minecraft(app: AppHandle, series_id: String) -> Result<MinecraftStatus, String> {
    tauri::async_runtime::spawn_blocking(move || launch_minecraft_sync(app, series_id))
        .await
        .map_err(|error| format!("Falló la tarea de inicio de Minecraft: {error}"))?
}

fn launch_minecraft_sync(app: AppHandle, series_id: String) -> Result<MinecraftStatus, String> {
    let series = current_catalog()?
        .series
        .into_iter()
        .find(|series| series.id == series_id)
        .ok_or_else(|| "La serie solicitada no existe en el catálogo".to_string())?;
    if series.pack_status != PackStatus::Available {
        return Err("Esta serie todavía no tiene un pack oficial publicado y no se puede iniciar como serie jugable.".into());
    }
    let mut settings = read_settings(&app)?;
    let session = app
        .state::<AuthSession>()
        .0
        .lock()
        .map_err(|_| "La sesión Microsoft quedó bloqueada".to_string())?
        .clone();
    let account = if let Some(mut session) = session {
        let client_id = configured_microsoft_client_id()
            .ok_or_else(|| "El inicio de sesión Microsoft no está habilitado en esta compilación.".to_string())?;
        let account = complete_refresh(client_id, None, &session.refresh_token)
            .map_err(|error| format!("La sesión Microsoft necesita renovarse. Vuelve a iniciar sesión: {error}"))?;
        let persistence_notice = save_microsoft_refresh_token(&account.refresh_token)
            .err()
            .map(|_| "La sesión Microsoft sigue activa, pero no se pudo guardar el token renovado; quizá debas iniciar sesión otra vez al reiniciar.".to_string());
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
        set_microsoft_restore_status(&app, persistence_notice)?;
        Account::Microsoft {
            username: account.name,
            uuid: account.id,
            access_token: account.access_token,
        }
    } else if settings.account_mode == Some(AccountMode::Offline) {
        let Some(username) = settings
            .offline_username
            .as_deref()
            .filter(|username| valid_offline_username(username))
        else {
            return Err("Configura un nombre válido para tu perfil local antes de jugar.".into());
        };
        let offline_uuid = offline_uuid_for_profile(
            username,
            settings.offline_username.as_deref(),
            settings.offline_uuid.as_deref(),
        );
        if settings.offline_uuid.as_deref() != Some(offline_uuid.as_str()) {
            update_settings(&app, |settings| {
                if settings.offline_uuid.as_deref() == Some(offline_uuid.as_str()) {
                    return Ok(false);
                }
                settings.offline_uuid = Some(offline_uuid.clone());
                Ok(true)
            })?;
        }
        Account::Offline {
            username: username.to_string(),
            uuid: offline_uuid,
        }
    } else {
        return Err("Inicia sesión con Microsoft o configura un perfil local con nombre para jugar.".into());
    };
    // Hold the instance lock before resolving its configured directory. Folder
    // changes use the same lock, so this launch cannot keep a stale path snapshot.
    let lock_root = app.path().app_data_dir().map_err(|error| error.to_string())?;
    let _pack_guard = pack::lock_pack_sync(&lock_root)?;
    ensure_series_not_running(&app, &series_id)?;
    migrate_managed_instance_for_series(&app, &series_id)?;
    let game_dir = resolve_isolated_series_game_directory(&app, &series_id)?
        .ok_or_else(|| "Selecciona o instala la carpeta de juego de esta serie primero.".to_string())?;
    ensure_game_directory_not_running(&game_dir)?;
    verify_forge_profile(&game_dir, &series)?;
    pack::verify_installed_pack_locked(&game_dir, &series_id)?;
    let profile_id = expected_profile_id(&series).ok_or_else(|| "No se pudo determinar el perfil del juego".to_string())?;
    let mut java = if let Some(path) = settings.java_executable.as_deref() {
        inspect_java(Path::new(path))
    } else {
        detect_java()
    };
    if !java.compatible && settings.java_executable.is_none() {
        let runtime_root = game_dir.clone();
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
                account,
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
    let mut game_process = Command::new(&command.executable);
    hide_console_if_windows(&mut game_process);
    let child = game_process
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
    let lock_root = app.path().app_data_dir().map_err(|error| error.to_string())?;
    let _guard = pack::lock_pack_sync(&lock_root)?;
    let game_directory = resolve_isolated_series_game_directory(&app, &series_id)?;
    mod_inventory_for_path(game_directory)
}

#[tauri::command]
fn add_personal_mod_to_series(app: AppHandle, series_id: String) -> Result<ModInventory, String> {
    resolve_isolated_series_game_directory(&app, &series_id)?
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
    let lock_root = app.path().app_data_dir().map_err(|error| error.to_string())?;
    let _guard = pack::lock_pack_sync(&lock_root)?;
    ensure_series_not_running(&app, &series_id)?;
    migrate_managed_instance_for_series(&app, &series_id)?;
    let game_directory = resolve_isolated_series_game_directory(&app, &series_id)?
        .ok_or_else(|| "La carpeta de juego de esta serie ya no está disponible".to_string())?;
    add_personal_mod_inner(&source, &game_directory.join("mods"))?;
    drop(_guard);
    get_mod_inventory(app, series_id)
}

#[tauri::command]
fn activate_personal_mod_for_series(
    app: AppHandle,
    series_id: String,
    file_name: String,
) -> Result<ModInventory, String> {
    let lock_root = app.path().app_data_dir().map_err(|error| error.to_string())?;
    let _guard = pack::lock_pack_sync(&lock_root)?;
    ensure_series_not_running(&app, &series_id)?;
    migrate_managed_instance_for_series(&app, &series_id)?;
    let game_directory = resolve_isolated_series_game_directory(&app, &series_id)?
        .ok_or_else(|| "No hay una carpeta de juego seleccionada para esta serie".to_string())?;
    activate_personal_mod_inner(&file_name, &game_directory.join("mods"))?;
    drop(_guard);
    get_mod_inventory(app, series_id)
}

#[tauri::command]
fn remove_personal_mod_from_series(
    app: AppHandle,
    series_id: String,
    file_name: String,
) -> Result<ModInventory, String> {
    let lock_root = app.path().app_data_dir().map_err(|error| error.to_string())?;
    let _guard = pack::lock_pack_sync(&lock_root)?;
    ensure_series_not_running(&app, &series_id)?;
    migrate_managed_instance_for_series(&app, &series_id)?;
    let game_directory = resolve_isolated_series_game_directory(&app, &series_id)?
        .ok_or_else(|| "No hay una carpeta de juego seleccionada para esta serie".to_string())?;
    remove_personal_mod_inner(&file_name, &game_directory.join("mods"))?;
    drop(_guard);
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
    update_settings(&app, |settings| {
        settings.java_executable = Some(path.to_string_lossy().into_owned());
        Ok(true)
    })?;
    make_bootstrap(&app)
}

#[tauri::command]
fn reset_java_selection(app: AppHandle) -> Result<Bootstrap, String> {
    update_settings(&app, |settings| {
        settings.java_executable = None;
        Ok(true)
    })?;
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
        let result: Result<(), String> = (|| -> Result<(), String> {
            // Forge comparte el directorio de la instancia con el pack y los mods.
            // El mismo bloqueo evita escrituras concurrentes y lanzar durante la instalación.
            let lock_root = app_handle.path().app_data_dir().map_err(|error| error.to_string())?;
            let _guard = pack::lock_pack_sync(&lock_root)?;
            ensure_series_not_running(&log_app, &log_series)?;
            let game_dir = migrate_managed_instance_for_series(&log_app, &log_series)?;
            ensure_game_directory_not_running(&game_dir)?;
            install_forge_profile(
                app_handle,
                series.id.clone(),
                series.minecraft_version,
                series.loader_version,
                java,
                game_dir,
            )?;
            // Persistir el perfil antes de liberar el bloqueo evita que un cambio
            // de carpeta o una acción Jugar vea la instalación a medio registrar.
            update_settings(&log_app, |settings| {
                settings
                    .managed_installs
                    .insert(managed_series_id, installed_profile_id);
                Ok(true)
            })?;
            Ok(())
        })();
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
    make_bootstrap(&app)
}

#[tauri::command]
fn set_active_series(app: AppHandle, series_id: String) -> Result<Bootstrap, String> {
    let catalog = current_catalog()?;
    if !catalog.series.iter().any(|series| series.id == series_id) {
        return Err("La serie solicitada no existe en el catálogo".into());
    }
    update_settings(&app, |settings| {
        settings.active_series_id = Some(series_id);
        Ok(true)
    })?;
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
    let lock_root = app.path().app_data_dir().map_err(|error| error.to_string())?;
    let _guard = pack::lock_pack_sync(&lock_root)?;
    ensure_series_not_running(&app, &series_id)?;
    ensure_game_directory_not_running(&chosen)?;
    update_settings(&app, |settings| {
        ensure_game_directory_isolated(&app, settings, &series_id, &chosen)?;
        settings.active_series_id = Some(series_id.clone());
        settings
            .game_directories
            .insert(series_id, chosen.to_string_lossy().into_owned());
        Ok(true)
    })?;
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
    let lock_root = app.path().app_data_dir().map_err(|error| error.to_string())?;
    let _guard = pack::lock_pack_sync(&lock_root)?;
    ensure_series_not_running(&app, &series_id)?;
    ensure_game_directory_not_running(&path)?;
    update_settings(&app, |settings| {
        ensure_game_directory_isolated(&app, settings, &series_id, &path)?;
        settings.active_series_id = Some(series_id.clone());
        settings
            .game_directories
            .insert(series_id, path.to_string_lossy().into_owned());
        Ok(true)
    })?;
    make_bootstrap(&app)
}

pub fn run() {
    tauri::Builder::default()
        .manage(AuthSession::default())
        .manage(MicrosoftSessionStatus::default())
        .manage(MinecraftProcess::default())
        .manage(ForgeInstallerProcess::default())
        .manage(developer::GitHubDeveloper::default())
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_updater::Builder::new().build())
        .plugin(tauri_plugin_process::init())
        .invoke_handler(tauri::generate_handler![
            get_bootstrap,
            set_theme,
            set_background_preset,
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
            login_microsoft,
            logout_microsoft,
            login_offline,
            logout_offline,
            developer::begin_github_developer_login,
            developer::poll_github_developer_login,
            developer::github_developer_status,
            developer::logout_github_developer,
            developer::choose_pack_source_directory,
            developer::refresh_pack_source_preview,
            developer::publish_pack_release,
            get_minecraft_status,
            launcher_updater_supported,
            open_launcher_releases,
            set_memory_limit,
            reset_memory_limit,
            launch_minecraft
        ])
        .build(tauri::generate_context!())
        .expect("error while building EternalCraft Launcher")
        .run(|app, event| {
            if matches!(event, tauri::RunEvent::Exit) {
                // Tauri termina el proceso con std::process::exit; los destructores
                // no son un lugar fiable para detener el hijo Forge.
                stop_forge_installer(app);
            }
        });
}

#[cfg(test)]
mod tests {
    #[test]
    fn catalog_refresh_uses_a_short_retry_without_cache_and_a_five_minute_cache_ttl() {
        use std::time::{Duration, Instant};

        let now = Instant::now() + Duration::from_secs(600);
        assert!(super::catalog_refresh_due(now, None, false, false));
        assert!(!super::catalog_refresh_due(
            now,
            Some(now - Duration::from_secs(29)),
            false,
            false
        ));
        assert!(super::catalog_refresh_due(
            now,
            Some(now - Duration::from_secs(30)),
            false,
            false
        ));
        assert!(!super::catalog_refresh_due(
            now,
            Some(now - Duration::from_secs(299)),
            true,
            true
        ));
        assert!(super::catalog_refresh_due(
            now,
            Some(now - Duration::from_secs(300)),
            true,
            true
        ));
        assert!(!super::catalog_refresh_due(
            now,
            Some(now - Duration::from_secs(29)),
            true,
            false
        ));
        assert!(super::catalog_refresh_due(
            now,
            Some(now - Duration::from_secs(30)),
            true,
            false
        ));
    }

    #[test]
    fn remote_catalog_body_limit_is_enforced_while_reading() {
        use std::io::Cursor;

        assert_eq!(
            super::read_bounded_body(Cursor::new(b"catalog".to_vec()), 7).unwrap(),
            b"catalog"
        );
        assert!(super::read_bounded_body(Cursor::new(b"too large".to_vec()), 8).is_err());
    }

    #[test]
    fn catalog_cache_round_trips_and_rejects_invalid_or_oversized_data() {
        use std::{
            fs,
            time::{SystemTime, UNIX_EPOCH},
        };

        let temp_dir = std::env::temp_dir().join(format!(
            "eternalcraft-catalog-cache-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos()
        ));
        let cache_path = temp_dir.join("series-catalog.json");
        let catalog = super::parse_catalog().expect("embedded catalog must be valid");

        super::write_catalog_cache(&cache_path, &catalog).expect("cache write must succeed");
        let restored = super::read_catalog_cache(&cache_path).expect("cache must validate");
        assert_eq!(restored.series.len(), catalog.series.len());

        fs::write(&cache_path, b"not json").unwrap();
        assert!(super::read_catalog_cache(&cache_path).is_none());

        fs::write(&cache_path, vec![b' '; super::MAX_CATALOG_BYTES + 1]).unwrap();
        assert!(super::read_catalog_cache(&cache_path).is_none());
        fs::remove_dir_all(temp_dir).unwrap();
    }

    #[test]
    fn pack_and_personal_mod_mutations_are_blocked_only_for_the_running_series() {
        let running = super::MinecraftStatus {
            running: true,
            series_id: Some("siege".into()),
            pid: Some(1234),
            exit_code: None,
            exit_success: None,
        };
        assert!(super::ensure_series_status_not_running(&running, "siege").is_err());
        assert!(super::ensure_series_status_not_running(&running, "ghouls-outbreak").is_ok());

        let unidentified = super::MinecraftStatus {
            series_id: None,
            ..running.clone()
        };
        assert!(super::ensure_series_status_not_running(&unidentified, "siege").is_err());

        let stopped = super::MinecraftStatus {
            running: false,
            ..running
        };
        assert!(super::ensure_series_status_not_running(&stopped, "siege").is_ok());
    }

    #[test]
    #[ignore = "subprocess fixture for forge_installer_is_killed_on_launcher_exit"]
    fn forge_installer_child_waits_for_shutdown() {
        std::thread::sleep(std::time::Duration::from_secs(60));
    }

    #[test]
    fn forge_installer_is_killed_on_launcher_exit() {
        use std::process::{Command, Stdio};
        use super::{ForgeInstallerProcess, ForgeInstallerState};

        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args([
                "--exact",
                "tests::forge_installer_child_waits_for_shutdown",
                "--ignored",
                "--nocapture",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let child = command.spawn()
            .expect("spawn the long-running fixture");
        let process = ForgeInstallerProcess(std::sync::Mutex::new(ForgeInstallerState {
            active: Some(child),
            shutting_down: false,
        }));
        assert!(super::terminate_registered_forge_installer(&process));
        let state = process.0.lock().unwrap();
        assert!(state.active.is_none());
        assert!(state.shutting_down);
        assert!(super::ensure_forge_installer_can_start(&state).is_err());
    }

    #[test]
    fn forge_installer_timeout_cancels_child_without_blocking_a_retry() {
        use std::process::{Command, Stdio};
        use super::{ForgeInstallerProcess, ForgeInstallerState};

        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "tests::forge_installer_child_waits_for_shutdown",
                "--ignored",
                "--nocapture",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let child = command.spawn().expect("spawn the long-running fixture");
        let process = ForgeInstallerProcess(std::sync::Mutex::new(ForgeInstallerState {
            active: Some(child),
            shutting_down: false,
        }));

        assert!(super::terminate_registered_forge_installer_after_timeout(&process));
        let state = process.0.lock().unwrap();
        assert!(state.active.is_none());
        assert!(!state.shutting_down);
        assert!(super::ensure_forge_installer_can_start(&state).is_ok());
    }

    #[test]
    fn forge_installer_deadline_is_bounded_to_twenty_minutes() {
        use std::time::{Duration, Instant};

        let started = Instant::now();
        assert!(!super::forge_install_timeout_expired(
            started,
            started + Duration::from_secs(20 * 60 - 1)
        ));
        assert!(super::forge_install_timeout_expired(
            started,
            started + Duration::from_secs(20 * 60)
        ));
    }

    #[test]
    fn minecraft_game_directory_argument_accepts_mojang_argument_shapes() {
        use std::ffi::OsString;
        assert_eq!(
            super::game_directory_argument(&[
                OsString::from("net.minecraft.client.main.Main"),
                OsString::from("--gameDir"),
                OsString::from("/games/siege"),
            ]),
            Some(std::path::PathBuf::from("/games/siege"))
        );
        assert_eq!(
            super::game_directory_argument(&[OsString::from("--gameDir=/games/ghouls")]),
            Some(std::path::PathBuf::from("/games/ghouls"))
        );
        assert_eq!(
            super::game_directory_argument(&[OsString::from("--assetsDir=/games/siege/assets")]),
            None
        );
    }

    #[test]
    fn java_process_name_recognizes_java_and_javaw_only() {
        use std::ffi::OsStr;
        assert!(super::java_process_name(OsStr::new("java")));
        assert!(super::java_process_name(OsStr::new("javaw.exe")));
        assert!(!super::java_process_name(OsStr::new("minecraft")));
        assert!(!super::java_process_name(OsStr::new("java-helper")));
    }

    #[cfg(windows)]
    #[test]
    fn java_child_process_has_no_console_and_keeps_captured_output() {
        let script = concat!(
            "Add-Type -TypeDefinition '",
            "using System; using System.Runtime.InteropServices; ",
            "public static class EternalCraftConsoleProbe { ",
            "[DllImport(\"kernel32.dll\")] public static extern IntPtr GetConsoleWindow(); }'; ",
            "if ([EternalCraftConsoleProbe]::GetConsoleWindow() -ne [IntPtr]::Zero) { exit 21 }; ",
            "[Console]::Write('hidden')"
        );
        let mut command = Command::new("powershell.exe");
        command
            .args(["-NoProfile", "-NonInteractive", "-Command", script])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        super::hide_console_if_windows(&mut command);
        let output = command.output().expect("PowerShell probe should start");
        assert!(
            output.status.success(),
            "PowerShell probe failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout), "hidden");
    }

    #[test]
    fn offline_username_accepts_minecraft_style_names() {
        assert!(super::valid_offline_username("Santi_17"));
        assert!(super::valid_offline_username("Abc"));
        assert!(super::valid_offline_username("A123456789012345"));
        assert!(super::valid_offline_uuid("54f3d715-9c21-4ab5-a321-123456789abc"));
    }

    #[test]
    fn offline_player_uuid_stays_stable_for_the_same_saved_profile() {
        let first = super::offline_uuid_for_profile("Santi_17", None, None);
        assert!(super::valid_offline_uuid(&first));
        assert_eq!(
            super::offline_uuid_for_profile("Santi_17", Some("Santi_17"), Some(&first)),
            first
        );
        assert_ne!(
            super::offline_uuid_for_profile("Other_17", Some("Santi_17"), Some(&first)),
            first
        );
    }

    #[test]
    fn offline_username_rejects_invalid_names() {
        for username in ["", "ab", "abcdefghijklmnopq", "two words", "name-1", "ñandú", "a.b"] {
            assert!(!super::valid_offline_username(username), "unexpectedly accepted {username:?}");
        }
        assert!(!super::valid_offline_uuid("not-a-uuid"));
    }

    use super::*;

    #[test]
    fn updater_is_limited_to_supported_installed_bundle_types() {
        assert!(update_supported_for("windows", false));
        assert!(update_supported_for("linux", true));
        assert!(!update_supported_for("linux", false));
        assert!(!update_supported_for("macos", false));
    }

    #[test]
    fn memory_limits_reserve_system_memory_and_cap_large_allocations() {
        assert_eq!(memory_bounds(512), (512, 512));
        assert_eq!(memory_bounds(768), (512, 512));
        assert_eq!(memory_bounds(4096), (1024, 3072));
        assert_eq!(memory_bounds(16 * 1024), (1024, 12 * 1024));
    }

    #[test]
    fn memory_budget_respects_linux_container_limits_when_present() {
        let host_total_mb = 32 * 1024;
        assert_eq!(effective_memory_mb(host_total_mb, None), host_total_mb);
        assert_eq!(
            effective_memory_mb(host_total_mb, Some(4 * 1024 * 1024 * 1024)),
            4 * 1024,
        );
        assert_eq!(
            effective_memory_mb(host_total_mb, Some(64 * 1024 * 1024 * 1024)),
            host_total_mb,
        );
        assert_eq!(effective_memory_mb(host_total_mb, Some(0)), host_total_mb);
    }

    #[test]
    fn cgroup_memory_budget_uses_the_tightest_parent_limit() {
        let fixture = std::env::temp_dir().join(format!(
            "eternalcraft-cgroup-memory-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let v2_mount = fixture.join("v2");
        let v1_mount = fixture.join("v1");
        fs::create_dir_all(v2_mount.join("system.slice/app.scope")).unwrap();
        fs::create_dir_all(v1_mount.join("user.slice/session.scope")).unwrap();
        fs::write(v2_mount.join("memory.max"), "max\n").unwrap();
        fs::write(v2_mount.join("system.slice/memory.max"), "4294967296\n").unwrap();
        fs::write(v2_mount.join("system.slice/app.scope/memory.max"), "6442450944\n").unwrap();
        fs::write(
            v1_mount.join("user.slice/memory.limit_in_bytes"),
            "3221225472\n",
        )
        .unwrap();
        fs::write(
            v1_mount.join("user.slice/session.scope/memory.limit_in_bytes"),
            "5368709120\n",
        )
        .unwrap();

        let membership = "0::/system.slice/app.scope\n5:memory:/user.slice/session.scope\n";
        assert_eq!(
            cgroup_memory_limit_from_membership(membership, &v2_mount, &v1_mount),
            Some(3 * 1024 * 1024 * 1024),
        );
        assert_eq!(
            cgroup_memory_limit_in_tree(&v2_mount, "/../../outside", "memory.max"),
            None,
        );
        assert_eq!(
            cgroup_memory_limit_in_tree(&v2_mount, "/system.slice", "../memory.max"),
            None,
        );

        fs::remove_dir_all(fixture).unwrap();
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
    fn microsoft_callback_reports_provider_denial_only_after_matching_state() {
        let denied = reqwest::Url::parse(
            "http://localhost:49152/?error=access_denied&state=expected",
        )
        .unwrap();
        assert_eq!(
            microsoft_oauth_callback_error(&denied, "expected"),
            Some("Inicio de sesión Microsoft cancelado o denegado. Puedes intentarlo de nuevo.")
        );
        assert_eq!(microsoft_oauth_callback_error(&denied, "attacker"), None);

        let failed = reqwest::Url::parse(
            "http://localhost:49152/?error=temporarily_unavailable&state=expected",
        )
        .unwrap();
        assert_eq!(
            microsoft_oauth_callback_error(&failed, "expected"),
            Some("Microsoft no pudo completar el inicio de sesión. Inténtalo de nuevo.")
        );

        let duplicate_state = reqwest::Url::parse(
            "http://localhost:49152/?error=access_denied&state=expected&state=attacker",
        )
        .unwrap();
        assert_eq!(microsoft_oauth_callback_error(&duplicate_state, "expected"), None);
    }

    #[test]
    fn microsoft_callback_listens_on_localhost_address_families_and_ignores_bad_state() {
        let (ipv4, ipv6) = bind_microsoft_callback_listeners().unwrap();
        let port = ipv4.local_addr().unwrap().port();
        assert!(ipv4.local_addr().unwrap().ip().is_loopback());
        if let Some(ipv6) = &ipv6 {
            let address = ipv6.local_addr().unwrap();
            assert!(address.ip().is_loopback());
            assert_eq!(address.port(), port);
        }

        let redirect = format!("http://localhost:{port}");
        for listener in std::iter::once(&ipv4).chain(ipv6.iter()) {
            let address = listener.local_addr().unwrap();
            let mut invalid = std::net::TcpStream::connect(address).unwrap();
            invalid
                .write_all(b"GET /?code=bad&state=wrong HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .unwrap();
            drop(invalid);
            assert!(accept_microsoft_callback(listener, &redirect, "expected")
                .unwrap()
                .is_none());

            let mut valid = std::net::TcpStream::connect(address).unwrap();
            valid
                .write_all(b"GET /?code=one-time-code&state=expected HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .unwrap();
            drop(valid);
            assert_eq!(
                accept_microsoft_callback(listener, &redirect, "expected").unwrap(),
                Some("one-time-code".into())
            );
        }
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

        let valid_mod = fixture.join("valid-mod.jar");
        let mut archive = zip::ZipWriter::new(fs::File::create(&valid_mod).unwrap());
        archive
            .start_file("META-INF/mods.toml", zip::write::SimpleFileOptions::default())
            .unwrap();
        archive
            .write_all(b"modLoader=\"javafml\"\nloaderVersion=\"[47,)\"\n[[mods]]\nmodId=\"valid_mod\"\nversion=\"1.0.0\"\n")
            .unwrap();
        archive.finish().unwrap();
        validate_forge_mod_archive(&valid_mod).unwrap();

        let malformed_mod = fixture.join("malformed-mod.jar");
        let mut archive = zip::ZipWriter::new(fs::File::create(&malformed_mod).unwrap());
        archive
            .start_file("META-INF/mods.toml", zip::write::SimpleFileOptions::default())
            .unwrap();
        archive
            .write_all(b"modLoader=\"javafml\"\nloaderVersion=\"[47,)\"\n")
            .unwrap();
        archive.finish().unwrap();
        assert!(validate_forge_mod_archive(&malformed_mod)
            .unwrap_err()
            .contains("no declara ningún mod Forge"));

        let invalid_id = fixture.join("invalid-id.jar");
        let mut archive = zip::ZipWriter::new(fs::File::create(&invalid_id).unwrap());
        archive
            .start_file("META-INF/mods.toml", zip::write::SimpleFileOptions::default())
            .unwrap();
        archive
            .write_all(b"modLoader=\"javafml\"\nloaderVersion=\"[47,)\"\n[[mods]]\nmodId=\"Bad-ID\"\n")
            .unwrap();
        archive.finish().unwrap();
        assert!(validate_forge_mod_archive(&invalid_id)
            .unwrap_err()
            .contains("modId ausente o no válido"));

        let missing_loader = fixture.join("missing-loader.jar");
        let mut archive = zip::ZipWriter::new(fs::File::create(&missing_loader).unwrap());
        archive
            .start_file("META-INF/mods.toml", zip::write::SimpleFileOptions::default())
            .unwrap();
        archive
            .write_all(b"modLoader=\"javafml\"\n[[mods]]\nmodId=\"valid_mod\"\n")
            .unwrap();
        archive.finish().unwrap();
        assert!(validate_forge_mod_archive(&missing_loader)
            .unwrap_err()
            .contains("loaderVersion válido"));

        let oversized_metadata = fixture.join("oversized-metadata.jar");
        let mut archive = zip::ZipWriter::new(fs::File::create(&oversized_metadata).unwrap());
        archive
            .start_file(
                "META-INF/mods.toml",
                zip::write::SimpleFileOptions::default(),
            )
            .unwrap();
        let mut metadata = b"modLoader=\"javafml\"\nloaderVersion=\"[47,)\"\n[[mods]]\nmodId=\"valid_mod\"\n".to_vec();
        metadata.resize(MAX_FORGE_MOD_METADATA_BYTES as usize + 1, b' ');
        archive.write_all(&metadata).unwrap();
        archive.finish().unwrap();
        assert!(validate_forge_mod_archive(&oversized_metadata)
            .unwrap_err()
            .contains("supera el tamaño máximo"));

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
    fn remote_catalog_rejects_case_colliding_ids_and_css_injection() {
        let mut catalog = parse_catalog().unwrap();
        let mut duplicate = catalog.series[0].clone();
        duplicate.id = "SIEGE".into();
        catalog.series.push(duplicate);
        assert!(validate_catalog(catalog).is_err());

        let mut catalog = parse_catalog().unwrap();
        catalog.series[0].accent = "#fff;background:url(https://example.invalid)".into();
        assert!(validate_catalog(catalog).is_err());
    }

    #[test]
    fn remote_catalog_rejects_unsupported_loaders_and_malformed_versions() {
        let mut catalog = parse_catalog().unwrap();
        catalog.series[0].loader = "Fabric".into();
        assert!(validate_catalog(catalog).is_err());

        let mut catalog = parse_catalog().unwrap();
        catalog.series[0].minecraft_version = "../../outside".into();
        assert!(validate_catalog(catalog).is_err());

        let mut catalog = parse_catalog().unwrap();
        catalog.series[0].name = "  \n".into();
        assert!(validate_catalog(catalog).is_err());
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
    fn forge_metadata_retries_transient_failures_and_stops_after_success() {
        let mut attempts = 0;
        let result = super::retry_request(3, std::time::Duration::ZERO, || {
            attempts += 1;
            if attempts < 3 {
                Err("temporary network failure")
            } else {
                Ok("valid checksum")
            }
        });

        assert_eq!(result.unwrap(), "valid checksum");
        assert_eq!(attempts, 3);
    }

    #[test]
    fn forge_metadata_retry_returns_the_last_error_after_attempt_limit() {
        let mut attempts = 0;
        let result = super::retry_request(2, std::time::Duration::ZERO, || {
            attempts += 1;
            Err::<(), _>(format!("network failure {attempts}"))
        });

        assert_eq!(result.unwrap_err(), "network failure 2");
        assert_eq!(attempts, 2);
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
        mojang_runtime::install_java_runtime(&runtime.component, &game_dir, &mut progress).unwrap();
        let java = get_executable_path(&runtime.component, &game_dir).unwrap();
        assert!(inspect_java(&java).compatible);
        install_version_files(&vanilla, &game_dir, &mut progress).unwrap();

        let forge_version = "1.20.1-47.4.10";
        let installer_url = installer_url(forge_version);
        let checksum = parse_sha1_sidecar(&fetch_forge_checksum_sidecar(&installer_url).unwrap())
            .unwrap();
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
        ensure_forge_launcher_profiles(&game_dir).unwrap();
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
        let profiles: serde_json::Value =
            serde_json::from_slice(&fs::read(game_dir.join("launcher_profiles.json")).unwrap())
                .unwrap();
        assert!(profiles["profiles"]
            .as_object()
            .unwrap()
            .values()
            .any(|profile| profile["lastVersionId"] == "1.20.1-forge-47.4.10"));
        fs::remove_dir_all(game_dir).unwrap();
    }

    #[test]
    fn game_directory_overlap_detects_equal_and_nested_instances() {
        let root = Path::new("/games/eternalcraft/siege");
        assert!(game_directories_overlap(root, root));
        assert!(game_directories_overlap(root, Path::new("/games/eternalcraft/siege/mods")));
        assert!(game_directories_overlap(Path::new("/games/eternalcraft"), root));
        assert!(!game_directories_overlap(root, Path::new("/games/eternalcraft/ghouls")));
        assert!(!game_directories_overlap(root, Path::new("/games/other")));
    }

    #[cfg(windows)]
    #[test]
    fn game_directory_overlap_is_case_insensitive_on_windows() {
        assert!(game_directories_overlap(
            Path::new(r"C:\Games\EternalCraft\Siege"),
            Path::new(r"c:\games\eternalcraft\siege\mods")
        ));
        assert!(!game_directories_overlap(
            Path::new(r"C:\Games\EternalCraft\Siege"),
            Path::new(r"C:\Games\EternalCraft\Ghouls")
        ));
    }

    #[test]
    fn game_directory_requires_an_existing_directory() {
        assert!(valid_game_dir(Path::new("/path/that/does/not/exist/eternalcraft")).is_err());
        assert!(valid_game_dir(Path::new(env!("CARGO_MANIFEST_DIR"))).is_ok());
    }

    #[test]
    fn managed_instance_moves_from_roaming_to_local_data_without_copying() {
        use std::fs;

        let fixture = std::env::temp_dir().join(format!(
            "eternalcraft-instance-migration-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let local = fixture.join("local");
        let roaming = fixture.join("roaming");
        let legacy_instance = roaming.join("instances/siege");
        fs::create_dir_all(legacy_instance.join("mods/personales")).unwrap();
        fs::write(legacy_instance.join("mods/personales/custom.jar"), b"user mod").unwrap();

        let resolved = super::migrate_managed_instance_dir(&local, &roaming, "siege").unwrap();

        assert_eq!(resolved, local.join("instances/siege"));
        assert_eq!(
            fs::read(resolved.join("mods/personales/custom.jar")).unwrap(),
            b"user mod"
        );
        assert!(!legacy_instance.exists());
        fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn managed_instance_prefers_existing_local_data_and_preserves_legacy_directory() {
        use std::fs;

        let fixture = std::env::temp_dir().join(format!(
            "eternalcraft-instance-preference-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let local_instance = fixture.join("local/instances/siege");
        let legacy_instance = fixture.join("roaming/instances/siege");
        fs::create_dir_all(&local_instance).unwrap();
        fs::create_dir_all(&legacy_instance).unwrap();

        let resolved = super::migrate_managed_instance_dir(
            &fixture.join("local"),
            &fixture.join("roaming"),
            "siege",
        )
        .unwrap();

        assert_eq!(resolved, local_instance);
        assert!(legacy_instance.is_dir());
        fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn managed_instance_keeps_legacy_path_when_local_storage_is_unavailable() {
        use std::fs;

        let fixture = std::env::temp_dir().join(format!(
            "eternalcraft-instance-fallback-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let local_root_file = fixture.join("local");
        fs::create_dir_all(fixture.join("roaming/instances/siege")).unwrap();
        fs::write(&local_root_file, b"local storage unavailable").unwrap();
        let legacy_instance = fixture.join("roaming/instances/siege");

        let resolved = super::migrate_managed_instance_dir(
            &local_root_file,
            &fixture.join("roaming"),
            "siege",
        )
        .unwrap();

        assert_eq!(resolved, legacy_instance);
        assert!(legacy_instance.is_dir());
        fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn forge_profile_scaffold_is_created_only_when_no_launcher_profile_exists() {
        let game_dir = std::env::temp_dir().join(format!(
            "eternalcraft-launcher-profiles-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&game_dir).unwrap();
        ensure_forge_launcher_profiles(&game_dir).unwrap();
        let profile_path = game_dir.join("launcher_profiles.json");
        let profiles: serde_json::Value =
            serde_json::from_slice(&fs::read(&profile_path).unwrap()).unwrap();
        assert!(profiles["profiles"].is_object());

        let existing = br#"{"profiles":{"keep":{"name":"Keep"}}}"#;
        fs::write(&profile_path, existing).unwrap();
        ensure_forge_launcher_profiles(&game_dir).unwrap();
        assert_eq!(fs::read(&profile_path).unwrap(), existing);
        fs::remove_dir_all(game_dir).unwrap();
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
            theme_migrated: true,
            background_preset: Some("ghouls-outbreak".into()),
            microsoft_client_id: None,
            memory_limit_mb: Some(4096),
            offline_username: Some("Player_17".into()),
            offline_uuid: Some("54f3d715-9c21-4ab5-a321-123456789abc".into()),
            account_mode: Some(AccountMode::Offline),
        };
        let encoded = serde_json::to_vec(&settings).expect("settings serialize");
        let decoded: Settings = serde_json::from_slice(&encoded).expect("settings deserialize");
        assert_eq!(decoded.active_series_id.as_deref(), Some("ghouls-outbreak"));
        assert_eq!(decoded.game_directories, settings.game_directories);
        assert_eq!(decoded.java_executable, settings.java_executable);
        assert_eq!(decoded.managed_installs, settings.managed_installs);
        assert_eq!(decoded.theme_id.as_deref(), Some("ghouls"));
        assert_eq!(decoded.background_preset.as_deref(), Some("ghouls-outbreak"));
        assert_eq!(decoded.memory_limit_mb, Some(4096));
        assert_eq!(decoded.offline_username.as_deref(), Some("Player_17"));
        assert_eq!(decoded.offline_uuid.as_deref(), Some("54f3d715-9c21-4ab5-a321-123456789abc"));
        assert_eq!(decoded.account_mode, Some(AccountMode::Offline));
    }

    #[test]
    fn legacy_microsoft_client_id_is_ignored_in_user_preferences() {
        let legacy = r#"{"microsoftClientId":"12345678-1234-4234-8234-123456789abc"}"#;
        let mut settings: Settings = serde_json::from_str(legacy).expect("legacy settings deserialize");
        assert!(migrate_microsoft_client_id_preference(&mut settings));
        assert!(!migrate_microsoft_client_id_preference(&mut settings));
        let encoded = serde_json::to_value(settings).expect("updated settings serialize");
        assert!(encoded.get("microsoftClientId").is_none());
    }

    #[test]
    fn legacy_github_app_client_id_is_ignored_in_user_preferences() {
        let legacy = r#"{"githubAppClientId":"Iv23liAbCdEfGh123456"}"#;
        let settings: Settings = serde_json::from_str(legacy).expect("legacy settings deserialize");
        let encoded = serde_json::to_value(settings).expect("updated settings serialize");
        assert!(encoded.get("githubAppClientId").is_none());
    }

    #[test]
    fn appearance_validation_accepts_supported_themes() {
        assert_eq!(resolve_theme(None), "arena-light");
        assert_eq!(resolve_theme(Some("ghouls".into())), "outbreak-night");
        assert_eq!(resolve_theme(Some("unknown".into())), "arena-light");
        assert!(is_valid_theme("series"));
        assert!(is_valid_theme("siege"));
        assert!(is_valid_theme("ghouls"));
        assert!(is_valid_theme("light"));
        for theme in [
            "arena-light", "siege-night", "ghouls-day", "outbreak-night",
            "glacier-light", "midnight-blue", "forest-mist", "deep-forest",
            "dune-light", "forge-dark", "rose-light", "eclipse",
            "ocean-light", "abyss", "lavender-light", "carbon",
        ] {
            assert!(is_valid_theme(theme), "theme should be accepted: {theme}");
        }
        assert!(!is_valid_theme("developer"));
        let old_preferences = r#"{"backgroundFile":"background-123.webp","themeId":"siege"}"#;
        let migrated: Settings = serde_json::from_str(old_preferences).unwrap();
        let rewritten = serde_json::to_value(migrated).unwrap();
        assert!(rewritten.get("backgroundFile").is_none());
    }

    #[test]
    fn legacy_default_theme_migrates_to_light_without_overwriting_explicit_series_themes() {
        let mut legacy_default = Settings {
            theme_id: Some("series".into()),
            ..Settings::default()
        };
        assert!(migrate_theme_preference(&mut legacy_default));
        assert_eq!(legacy_default.theme_id.as_deref(), Some("arena-light"));
        assert!(legacy_default.theme_migrated);
        assert!(!migrate_theme_preference(&mut legacy_default));

        let mut explicit_theme = Settings {
            theme_id: Some("ghouls".into()),
            ..Settings::default()
        };
        assert!(migrate_theme_preference(&mut explicit_theme));
        assert_eq!(explicit_theme.theme_id.as_deref(), Some("ghouls"));
    }

    #[test]
    fn only_bundled_series_background_presets_are_accepted() {
        assert!(is_valid_background_preset("siege"));
        assert!(is_valid_background_preset("ghouls-outbreak"));
        assert!(!is_valid_background_preset("https://attacker.invalid/image.png"));
        assert!(!is_valid_background_preset("unknown"));
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

    #[cfg(windows)]
    #[test]
    fn windows_java_discovery_includes_standard_jdk_and_oracle_paths() {
        let root = std::env::temp_dir().join(format!(
            "eternalcraft-java-paths-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let local_app_data = root.join("LocalAppData");
        let vendor_root = root.join("Program Files/Eclipse Adoptium");
        let zulu_root = root.join("Program Files/Azul Systems/Zulu");
        let oracle_root = root.join("Program Files/Common Files/Oracle/Java/javapath");
        let winget_root = local_app_data.join("Microsoft/WinGet/Links");
        std::fs::create_dir_all(vendor_root.join("jdk-17/bin")).unwrap();
        std::fs::create_dir_all(zulu_root.join("zulu-17/bin")).unwrap();
        std::fs::create_dir_all(&oracle_root).unwrap();
        std::fs::create_dir_all(&winget_root).unwrap();
        std::fs::write(vendor_root.join("jdk-17/bin/java.exe"), b"fixture").unwrap();
        std::fs::write(zulu_root.join("zulu-17/bin/java.exe"), b"fixture").unwrap();
        std::fs::write(oracle_root.join("java.exe"), b"fixture").unwrap();
        std::fs::write(winget_root.join("java.exe"), b"fixture").unwrap();

        let candidates = super::windows_java_candidates_from_roots(
            &[root.join("Program Files")],
            Some(&local_app_data),
        );

        assert!(candidates.contains(&vendor_root.join("jdk-17/bin/java.exe")));
        assert!(candidates.contains(&zulu_root.join("zulu-17/bin/java.exe")));
        assert!(candidates.contains(&oracle_root.join("java.exe")));
        assert!(candidates.contains(&winget_root.join("java.exe")));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn java_version_probe_stops_a_hung_executable() {
        use std::os::unix::fs::PermissionsExt;

        let directory = std::env::temp_dir().join(format!(
            "eternalcraft-java-probe-hangs-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let executable = directory.join("java-hangs");
        std::fs::write(&executable, "#!/bin/sh\nexec sleep 30\n").unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();

        let started = std::time::Instant::now();
        let error = super::java_version_output(&executable, std::time::Duration::from_millis(100))
            .unwrap_err();

        assert!(error.contains("superó el límite"));
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn java_version_probe_does_not_wait_for_orphaned_pipe_holders() {
        use std::os::unix::fs::PermissionsExt;

        let directory = std::env::temp_dir().join(format!(
            "eternalcraft-java-probe-orphan-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let executable = directory.join("java-orphan");
        std::fs::write(
            &executable,
            "#!/bin/sh\nsleep 30 &\necho 'openjdk version \"17.0.15\"' >&2\nexit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();

        let started = std::time::Instant::now();
        let error = super::java_version_output(&executable, std::time::Duration::from_millis(100))
            .unwrap_err();

        assert!(error.contains("dejó abierta su salida"));
        assert!(started.elapsed() < std::time::Duration::from_secs(2));
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn java_version_probe_captures_the_version_from_stderr() {
        use std::os::unix::fs::PermissionsExt;

        let directory = std::env::temp_dir().join(format!(
            "eternalcraft-java-probe-output-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir(&directory).unwrap();
        let executable = directory.join("java-stub");
        std::fs::write(
            &executable,
            "#!/bin/sh\necho 'openjdk version \"17.0.15\"' >&2\n",
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();

        let output =
            super::java_version_output(&executable, std::time::Duration::from_secs(1)).unwrap();

        assert!(output.status.success());
        assert_eq!(
            super::parse_java_major(&String::from_utf8_lossy(&output.stderr)),
            Some(("17.0.15".into(), 17))
        );
        std::fs::remove_dir_all(directory).unwrap();
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
            .write_all(b"modLoader=\"javafml\"\nloaderVersion=\"[47,)\"\n[[mods]]\nmodId=\"custom_mod\"\nversion=\"1.0.0\"\n")
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
        archive
            .write_all(b"modLoader=\"javafml\"\nloaderVersion=\"[47,)\"\n[[mods]]\nmodId=\"taken_mod\"\nversion=\"1.0.0\"\n")
            .unwrap();
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
    fn limited_copy_removes_partial_files_when_the_source_exceeds_the_limit() {
        let fixture = std::env::temp_dir().join(format!(
            "eternalcraft-limited-copy-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&fixture).unwrap();
        let source = fixture.join("source.jar");
        let destination = fixture.join("destination.jar");
        fs::write(&source, b"1234567").unwrap();

        let error = copy_file_create_new_limited(&source, &destination, 6).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(!destination.exists());
        fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn personal_mod_import_rejects_oversized_source_before_opening_the_jar() {
        let fixture = std::env::temp_dir().join(format!(
            "eternalcraft-personal-size-check-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&fixture).unwrap();
        let source = fixture.join("oversized.jar");
        let mods = fixture.join("game/mods");
        fs::write(&source, b"not a valid jar").unwrap();

        let error = add_personal_mod_inner_with_limit(&source, &mods, 6).unwrap_err();
        assert!(error.contains("6 bytes"));
        assert!(!mods.exists());
        fs::remove_dir_all(fixture).unwrap();
    }

    #[test]
    fn personal_mod_import_uses_the_official_mod_size_limit() {
        assert!(validate_personal_mod_size(pack::MOD_LIMIT, pack::MOD_LIMIT).is_ok());
        let error = validate_personal_mod_size(pack::MOD_LIMIT + 1, pack::MOD_LIMIT).unwrap_err();
        assert!(error.contains("512 MiB"));
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
        archive
            .write_all(b"modLoader=\"javafml\"\nloaderVersion=\"[47,)\"\n[[mods]]\nmodId=\"occupied_mod\"\nversion=\"1.0.0\"\n")
            .unwrap();
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

#[cfg(test)]
mod forge_mod_archive_directory_tests {
    use super::*;
    use std::io::Cursor;

    fn eocd(entries: u16, directory_bytes: u32) -> Cursor<Vec<u8>> {
        let mut bytes = b"PK\x05\x06".to_vec();
        bytes.extend_from_slice(&[0; 4]); // disk numbers
        bytes.extend_from_slice(&entries.to_le_bytes());
        bytes.extend_from_slice(&entries.to_le_bytes());
        bytes.extend_from_slice(&directory_bytes.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        Cursor::new(bytes)
    }

    #[test]
    fn accepts_a_normal_forge_mod_archive() {
        use std::io::Write;
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file("META-INF/mods.toml", zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"modLoader = \"javafml\"").unwrap();
        let mut archive = writer.finish().unwrap();
        assert!(check_forge_mod_archive_directory(&mut archive).is_ok());
    }

    #[test]
    fn accepts_archives_within_entry_and_directory_limits() {
        assert!(check_forge_mod_archive_directory(&mut eocd(50_000, 32 * 1024 * 1024)).is_ok());
    }

    #[test]
    fn rejects_archive_entry_and_central_directory_bombs_before_zip_parsing() {
        assert!(check_forge_mod_archive_directory(&mut eocd(50_001, 0)).is_err());
        assert!(check_forge_mod_archive_directory(&mut eocd(1, 32 * 1024 * 1024 + 1)).is_err());
    }

    #[test]
    fn rejects_zip64_and_incomplete_directories_before_zip_parsing() {
        assert!(check_forge_mod_archive_directory(&mut eocd(u16::MAX, 0)).is_err());
        assert!(check_forge_mod_archive_directory(&mut Cursor::new(b"PK\x05\x06".to_vec())).is_err());
    }
}

#[cfg(test)]
mod microsoft_credential_tests {
    use super::*;

    #[test]
    fn microsoft_refresh_token_can_be_saved_loaded_and_deleted() {
        keyring::set_default_credential_builder(keyring::mock::default_credential_builder());
        let token = "test-refresh-token-never-used-for-auth";
        let entry = CredentialEntry::new(MICROSOFT_CREDENTIAL_SERVICE, MICROSOFT_CREDENTIAL_USER)
            .unwrap();
        save_refresh_token_with_entry(&entry, token).unwrap();
        assert_eq!(load_refresh_token_with_entry(&entry).unwrap().unwrap().as_str(), token);
        delete_refresh_token_with_entry(&entry).unwrap();
        assert!(load_refresh_token_with_entry(&entry).unwrap().is_none());
    }
}
