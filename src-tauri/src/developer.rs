use super::*;
use sha2::Digest;
use std::collections::BTreeSet;
use std::time::{Duration, Instant};

const GITHUB_DEVICE_CODE_URL: &str = "https://github.com/login/device/code";
const GITHUB_ACCESS_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";
const GITHUB_DEVICE_VERIFICATION_URL: &str = "https://github.com/login/device";
const GITHUB_API: &str = "https://api.github.com";
const PUBLISH_REPOSITORY: &str = "Santi-PdR/EternalCraft-Launcher";
const MAX_SOURCE_JAR_BYTES: u64 = 256 * 1024 * 1024;
const MAX_SOURCE_PACK_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_GITHUB_ASSETS_PER_RELEASE: usize = 1000;

#[derive(Default)]
pub(super) struct GitHubDeveloper(std::sync::Mutex<DeveloperState>);

#[derive(Default)]
struct DeveloperState {
    pending: Option<PendingDeviceAuthorization>,
    session: Option<GitHubSession>,
    source_directories: BTreeMap<String, PathBuf>,
    publish_active: bool,
}

struct PublishLease {
    app: AppHandle,
}

fn reserve_publish(active: &mut bool) -> bool {
    if *active {
        false
    } else {
        *active = true;
        true
    }
}

impl PublishLease {
    fn acquire(app: &AppHandle) -> Result<Self, String> {
        let developer = app.state::<GitHubDeveloper>();
        let mut state = developer
            .0
            .lock()
            .map_err(|_| "El estado developer quedó bloqueado".to_string())?;
        if !reserve_publish(&mut state.publish_active) {
            return Err("Ya hay una publicación en curso. Espera a que termine antes de iniciar otra.".into());
        }
        Ok(Self { app: app.clone() })
    }
}

impl Drop for PublishLease {
    fn drop(&mut self) {
        let developer = self.app.state::<GitHubDeveloper>();
        if let Ok(mut state) = developer.0.lock() {
            state.publish_active = false;
        };
    }
}

struct PendingDeviceAuthorization {
    client_id: String,
    device_code: String,
    user_code: String,
    interval: Duration,
    expires_at: Instant,
    next_poll_at: Instant,
}

struct GitHubSession {
    username: String,
    access_token: String,
    expires_at: Option<Instant>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct DeveloperLoginStatus {
    status: String,
    username: Option<String>,
    user_code: Option<String>,
    verification_uri: Option<String>,
    expires_in_seconds: Option<u64>,
    interval_seconds: Option<u64>,
    message: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PackSourceFile {
    name: String,
    size_bytes: u64,
    sha256: String,
    license: Option<String>,
    license_status: PackLicenseStatus,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
enum PackLicenseStatus {
    Recognized,
    PermissionRequired,
    Unknown,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PackSourcePreview {
    series_id: String,
    directory: String,
    files: Vec<PackSourceFile>,
    total_bytes: u64,
    source_fingerprint: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct PackPublishProgress {
    series_id: String,
    completed_files: usize,
    total_files: usize,
    message: String,
}

#[derive(Deserialize)]
struct GitHubRelease {
    id: u64,
    draft: bool,
    upload_url: String,
}

#[derive(Clone, Deserialize)]
struct GitHubReleaseAsset {
    id: u64,
    name: String,
    digest: Option<String>,
}

#[derive(Deserialize)]
struct GitHubContentResponse {
    sha: String,
    content: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    schema_version: u32,
    series_id: String,
    version: String,
    minecraft_version: String,
    loader: String,
    loader_version: String,
    files: Vec<ManifestFile>,
}

#[derive(Serialize)]
struct ManifestFile {
    path: String,
    url: String,
    size_bytes: u64,
    sha256: String,
}

#[derive(Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: Option<u64>,
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    expires_in: Option<u64>,
    error: Option<String>,
    error_description: Option<String>,
}

#[derive(Deserialize)]
struct GitHubUser {
    login: String,
}

#[derive(Deserialize)]
struct RepositoryPermissions {
    push: Option<bool>,
    admin: Option<bool>,
}

#[derive(Deserialize)]
struct UserInstallationsResponse {
    installations: Vec<GitHubInstallation>,
}

#[derive(Deserialize)]
struct GitHubInstallation {
    id: u64,
    account: GitHubAccount,
    permissions: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct GitHubAccount {
    login: String,
}

#[derive(Deserialize)]
struct InstallationRepositoriesResponse {
    repositories: Vec<InstallationRepository>,
}

#[derive(Deserialize)]
struct InstallationRepository {
    full_name: String,
    permissions: Option<RepositoryPermissions>,
}

impl GitHubDeveloper {
    pub(super) fn username(&self) -> Option<String> {
        self.0
            .lock()
            .ok()
            .and_then(|state| {
                state.session.as_ref().and_then(|session| {
                    (!session.expires_at.is_some_and(|deadline| Instant::now() >= deadline))
                        .then(|| session.username.clone())
                })
            })
    }

    fn access_token(&self) -> Result<String, String> {
        let state = self
            .0
            .lock()
            .map_err(|_| "El estado de autorización GitHub quedó bloqueado".to_string())?;
        let session = state
            .session
            .as_ref()
            .ok_or_else(|| "Inicia sesión con GitHub y permiso de escritura para publicar".to_string())?;
        if session.expires_at.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err("La sesión de GitHub expiró. Vuelve a autorizar el launcher".into());
        }
        Ok(session.access_token.clone())
    }
}

fn valid_github_app_client_id(value: &str) -> bool {
    (10..=100).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn configured_github_app_client_id() -> Option<&'static str> {
    option_env!("ETERNALCRAFT_GITHUB_APP_CLIENT_ID")
        .filter(|value| valid_github_app_client_id(value))
}

pub(super) fn github_app_client_id_configured() -> bool {
    configured_github_app_client_id().is_some()
}

#[tauri::command]
pub(super) fn begin_github_developer_login(app: AppHandle) -> Result<DeveloperLoginStatus, String> {
    let client_id = configured_github_app_client_id()
        .ok_or_else(|| "El acceso Developer no está configurado para esta compilación".to_string())?;
    let client = http::client().map_err(|error| error.to_string())?;
    let response = client
        .post(GITHUB_DEVICE_CODE_URL)
        .header("Accept", "application/json")
        .header("User-Agent", "EternalCraft-Launcher")
        .form(&[("client_id", client_id.to_string())])
        .send()
        .map_err(|error| format!("No se pudo solicitar autorización a GitHub: {error}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        return Err(format!("GitHub rechazó el inicio de sesión ({status}): {}", body.chars().take(300).collect::<String>()));
    }
    let device: DeviceCodeResponse = response
        .json()
        .map_err(|error| format!("GitHub devolvió una respuesta de autorización inválida: {error}"))?;
    if device.device_code.is_empty()
        || device.user_code.is_empty()
        || !device.verification_uri.starts_with("https://github.com/")
        || device.expires_in == 0
        || device.expires_in > 900
    {
        return Err("GitHub devolvió datos de dispositivo inválidos".into());
    }
    let interval = Duration::from_secs(device.interval.unwrap_or(5).clamp(5, 60));
    let now = Instant::now();
    let expires_at = now + Duration::from_secs(device.expires_in);
    app.state::<GitHubDeveloper>()
        .0
        .lock()
        .map_err(|_| "El estado de autorización GitHub quedó bloqueado".to_string())?
        .pending = Some(PendingDeviceAuthorization {
            client_id,
            device_code: device.device_code,
            user_code: device.user_code.clone(),
        interval,
        expires_at,
        next_poll_at: now + interval,
    });
    let _ = open_system_browser(GITHUB_DEVICE_VERIFICATION_URL);
    Ok(DeveloperLoginStatus {
        status: "pending".into(),
        username: None,
        user_code: Some(device.user_code),
        verification_uri: Some(GITHUB_DEVICE_VERIFICATION_URL.into()),
        expires_in_seconds: Some(device.expires_in),
        interval_seconds: Some(interval.as_secs()),
        message: Some("Autoriza la aplicación con tu cuenta GitHub. El permiso se comprueba en el repositorio.".into()),
    })
}

#[tauri::command]
pub(super) fn poll_github_developer_login(
    app: AppHandle,
) -> Result<DeveloperLoginStatus, String> {
    let developer = app.state::<GitHubDeveloper>();
    let mut state = developer
        .0
        .lock()
        .map_err(|_| "El estado de autorización GitHub quedó bloqueado".to_string())?;
    let pending = state
        .pending
        .as_mut()
        .ok_or_else(|| "No hay una autorización GitHub pendiente".to_string())?;
    let now = Instant::now();
    if now >= pending.expires_at {
        state.pending = None;
        return Ok(login_status("expired", Some("El código de GitHub venció; inicia el proceso otra vez.")));
    }
    if now < pending.next_poll_at {
        return Ok(pending_status(pending, None));
    }
    pending.next_poll_at = now + pending.interval;
    let client_id = pending.client_id.clone();
    let device_code = pending.device_code.clone();
    let client = http::client().map_err(|error| error.to_string())?;
    let response = client
        .post(GITHUB_ACCESS_TOKEN_URL)
        .header("Accept", "application/json")
        .header("User-Agent", "EternalCraft-Launcher")
        .form(&[
            ("client_id", client_id.as_str()),
            ("device_code", device_code.as_str()),
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
        ])
        .send()
        .map_err(|error| format!("No se pudo consultar la autorización GitHub: {error}"))?;
    let token: TokenResponse = response
        .json()
        .map_err(|error| format!("GitHub devolvió un estado de autorización inválido: {error}"))?;
    match token.error.as_deref() {
        Some("authorization_pending") => {
            return Ok(pending_status(
                state.pending.as_ref().expect("pending device login exists"),
                None,
            ));
        }
        Some("slow_down") => {
            if let Some(pending) = state.pending.as_mut() {
                pending.interval = pending.interval.saturating_add(Duration::from_secs(5));
                pending.next_poll_at = Instant::now() + pending.interval;
            }
            return Ok(pending_status(
                state.pending.as_ref().expect("pending device login exists"),
                Some("GitHub pidió espaciar las consultas; el launcher ajustó el intervalo."),
            ));
        }
        Some("access_denied") => {
            state.pending = None;
            return Ok(login_status("denied", Some("Se rechazó la autorización de GitHub.")));
        }
        Some("expired_token") => {
            state.pending = None;
            return Ok(login_status("expired", Some("El código de GitHub venció; inicia el proceso otra vez.")));
        }
        Some(error) => {
            state.pending = None;
            return Ok(login_status("failed", Some(token.error_description.as_deref().unwrap_or(error))));
        }
        None => {}
    }
    let access_token = token
        .access_token
        .ok_or_else(|| "GitHub no devolvió un token de acceso".to_string())?;
    let expires_at = token.expires_in.map(|seconds| Instant::now() + Duration::from_secs(seconds));
    drop(state);

    let user = match get_github_user(&access_token)
        .and_then(|user| ensure_repository_write_access(&access_token).map(|()| user))
    {
        Ok(user) => user,
        Err(message) => {
            if let Ok(mut state) = developer.0.lock() {
                state.pending = None;
            }
            return Ok(login_status("failed", Some(&message)));
        }
    };
    let mut state = developer
        .0
        .lock()
        .map_err(|_| "El estado de autorización GitHub quedó bloqueado".to_string())?;
    state.pending = None;
    state.session = Some(GitHubSession {
        username: user.login.clone(),
        access_token,
        expires_at,
    });
    Ok(DeveloperLoginStatus {
        status: "authorized".into(),
        username: Some(user.login),
        user_code: None,
        verification_uri: None,
        expires_in_seconds: None,
        interval_seconds: None,
        message: Some("GitHub confirmó permiso de escritura para EternalCraft-Launcher.".into()),
    })
}

#[tauri::command]
pub(super) fn github_developer_status(app: AppHandle) -> DeveloperLoginStatus {
    let username = app.state::<GitHubDeveloper>().username();
    DeveloperLoginStatus {
        status: if username.is_some() { "authorized" } else { "signedOut" }.into(),
        username,
        user_code: None,
        verification_uri: None,
        expires_in_seconds: None,
        interval_seconds: None,
        message: None,
    }
}

#[tauri::command]
pub(super) fn logout_github_developer(app: AppHandle) -> Result<DeveloperLoginStatus, String> {
    let developer = app.state::<GitHubDeveloper>();
    let mut state = developer
        .0
        .lock()
        .map_err(|_| "El estado de autorización GitHub quedó bloqueado".to_string())?;
    state.pending = None;
    state.session = None;
    state.source_directories.clear();
    Ok(login_status("signedOut", Some("Se cerró la sesión developer de este proceso.")))
}

fn login_status(status: &str, message: Option<&str>) -> DeveloperLoginStatus {
    DeveloperLoginStatus {
        status: status.into(),
        username: None,
        user_code: None,
        verification_uri: None,
        expires_in_seconds: None,
        interval_seconds: None,
        message: message.map(str::to_string),
    }
}

fn pending_status(
    pending: &PendingDeviceAuthorization,
    message: Option<&str>,
) -> DeveloperLoginStatus {
    DeveloperLoginStatus {
        status: "pending".into(),
        username: None,
        user_code: Some(pending.user_code.clone()),
        verification_uri: Some(GITHUB_DEVICE_VERIFICATION_URL.into()),
        expires_in_seconds: Some(
            pending
                .expires_at
                .saturating_duration_since(Instant::now())
                .as_secs(),
        ),
        interval_seconds: Some(pending.interval.as_secs()),
        message: message.map(str::to_string),
    }
}

fn get_github_user(access_token: &str) -> Result<GitHubUser, String> {
    let client = http::client().map_err(|error| error.to_string())?;
    let response = client
        .get(format!("{GITHUB_API}/user"))
        .bearer_auth(access_token)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", "EternalCraft-Launcher")
        .send()
        .map_err(|error| format!("No se pudo validar la cuenta GitHub: {error}"))?;
    if !response.status().is_success() {
        return Err("GitHub no pudo validar esta sesión".into());
    }
    response
        .json()
        .map_err(|error| format!("GitHub devolvió una cuenta inválida: {error}"))
}

fn installation_has_contents_write(permissions: &BTreeMap<String, String>) -> bool {
    permissions
        .get("contents")
        .is_some_and(|permission| permission.eq_ignore_ascii_case("write"))
}

fn repository_permissions_allow_write(permissions: Option<RepositoryPermissions>) -> bool {
    permissions.is_some_and(|permissions| {
        permissions.push == Some(true) || permissions.admin == Some(true)
    })
}

fn ensure_repository_write_access(access_token: &str) -> Result<(), String> {
    let client = http::client().map_err(|error| error.to_string())?;
    let mut installations = Vec::new();
    for page in 1..=10 {
        let response = client
            .get(format!("{GITHUB_API}/user/installations?per_page=100&page={page}"))
            .bearer_auth(access_token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .header("User-Agent", "EternalCraft-Launcher")
            .send()
            .map_err(|error| format!("No se pudieron validar las instalaciones de la GitHub App: {error}"))?;
        if !response.status().is_success() {
            return Err(format!(
                "GitHub no pudo validar la instalación de la App ({})",
                response.status()
            ));
        }
        let result: UserInstallationsResponse = response
            .json()
            .map_err(|error| format!("GitHub devolvió instalaciones inválidas: {error}"))?;
        let count = result.installations.len();
        installations.extend(result.installations);
        if count < 100 {
            break;
        }
    }

    for installation in installations.into_iter().filter(|installation| {
        installation.account.login.eq_ignore_ascii_case("Santi-PdR")
            && installation_has_contents_write(&installation.permissions)
    }) {
        for page in 1..=10 {
            let response = client
                .get(format!(
                    "{GITHUB_API}/user/installations/{}/repositories?per_page=100&page={page}",
                    installation.id
                ))
                .bearer_auth(access_token)
                .header("Accept", "application/vnd.github+json")
                .header("X-GitHub-Api-Version", "2022-11-28")
                .header("User-Agent", "EternalCraft-Launcher")
                .send()
                .map_err(|error| format!("No se pudo comprobar el acceso de la GitHub App al repositorio: {error}"))?;
            if !response.status().is_success() {
                return Err(format!(
                    "GitHub no pudo comprobar los repositorios de la instalación ({})",
                    response.status()
                ));
            }
            let result: InstallationRepositoriesResponse = response
                .json()
                .map_err(|error| format!("GitHub devolvió repositorios instalados inválidos: {error}"))?;
            let count = result.repositories.len();
            if let Some(repository) = result.repositories.into_iter().find(|repository| {
                repository.full_name.eq_ignore_ascii_case(PUBLISH_REPOSITORY)
            }) {
                if repository_permissions_allow_write(repository.permissions) {
                    return Ok(());
                }
                return Err("Tu cuenta no tiene permiso de escritura en EternalCraft-Launcher".into());
            }
            if count < 100 {
                break;
            }
        }
    }

    Err("Instala la GitHub App con permiso Contents: Read and write únicamente en Santi-PdR/EternalCraft-Launcher.".into())
}

#[tauri::command]
pub(super) fn choose_pack_source_directory(
    app: AppHandle,
    series_id: String,
) -> Result<Option<PackSourcePreview>, String> {
    let _token = app.state::<GitHubDeveloper>().access_token()?;
    let catalog = current_catalog()?;
    if !catalog.series.iter().any(|series| series.id == series_id) {
        return Err("La serie solicitada no existe en el catálogo".into());
    }
    let selected = app
        .dialog()
        .file()
        .set_title("Seleccionar carpeta con mods JAR oficiales")
        .blocking_pick_folder();
    let Some(selected) = selected else {
        return Ok(None);
    };
    let path = selected
        .into_path()
        .map_err(|error| format!("Ruta de carpeta no válida: {error}"))?;
    let preview = scan_pack_source(&path, &series_id)?;
    let canonical_path = PathBuf::from(&preview.directory);
    app.state::<GitHubDeveloper>()
        .0
        .lock()
        .map_err(|_| "El estado developer quedó bloqueado".to_string())?
        .source_directories
        .insert(series_id, canonical_path);
    Ok(Some(preview))
}

#[tauri::command]
pub(super) fn refresh_pack_source_preview(
    app: AppHandle,
    series_id: String,
) -> Result<PackSourcePreview, String> {
    let _token = app.state::<GitHubDeveloper>().access_token()?;
    let path = app
        .state::<GitHubDeveloper>()
        .0
        .lock()
        .map_err(|_| "El estado developer quedó bloqueado".to_string())?
        .source_directories
        .get(&series_id)
        .cloned()
        .ok_or_else(|| "Selecciona primero una carpeta fuente para esta serie".to_string())?;
    scan_pack_source(&path, &series_id)
}

#[tauri::command]
pub(super) async fn publish_pack_release(
    app: AppHandle,
    series_id: String,
    version: String,
    confirmed_source_fingerprint: String,
) -> Result<String, String> {
    tauri::async_runtime::spawn_blocking(move || {
        publish_pack_release_sync(app, series_id, version, confirmed_source_fingerprint)
    })
        .await
        .map_err(|error| format!("La tarea de publicación se interrumpió: {error}"))?
}

fn publish_pack_release_sync(
    app: AppHandle,
    series_id: String,
    version: String,
    confirmed_source_fingerprint: String,
) -> Result<String, String> {
    let _publish_lease = PublishLease::acquire(&app)?;
    let access_token = app.state::<GitHubDeveloper>().access_token()?;
    if !valid_pack_version(&version) {
        return Err("Usa una versión estable con formato MAJOR.MINOR.PATCH, por ejemplo 1.2.0".into());
    }
    let catalog = current_catalog()?;
    let series = catalog
        .series
        .iter()
        .find(|series| series.id == series_id)
        .ok_or_else(|| "La serie seleccionada no existe".to_string())?;
    let source = app
        .state::<GitHubDeveloper>()
        .0
        .lock()
        .map_err(|_| "El estado developer quedó bloqueado".to_string())?
        .source_directories
        .get(&series_id)
        .cloned()
        .ok_or_else(|| "Selecciona primero la carpeta fuente de mods de esta serie".to_string())?;
    let source = scan_pack_source(&source, &series_id)?;
    if source.source_fingerprint != confirmed_source_fingerprint {
        return Err("La carpeta o sus JAR cambiaron desde la revisión de licencias. Vuelve a verificar la fuente y confirma otra vez.".into());
    }
    if source.files.len() > MAX_GITHUB_ASSETS_PER_RELEASE {
        return Err(format!("GitHub permite hasta {MAX_GITHUB_ASSETS_PER_RELEASE} assets por release"));
    }

    let tag = format!("{series_id}-v{version}");
    let release = get_or_create_draft_release(&access_token, &tag, &series.name)?;
    let assets = list_release_assets(&access_token, release.id)?;
    if !release.draft && !published_asset_names_match(&source.files, &assets) {
        return Err(format!(
            "El release publicado {tag} ya tiene un conjunto de mods distinto; crea una versión nueva"
        ));
    }
    let mut verified_names = std::collections::BTreeSet::new();
    let total = source.files.len();
    for (index, source_file) in source.files.iter().enumerate() {
        let expected_digest = format!("sha256:{}", source_file.sha256);
        if let Some(asset) = assets.iter().find(|asset| asset.name == source_file.name) {
            if asset.digest.as_deref() == Some(expected_digest.as_str()) {
                verified_names.insert(source_file.name.clone());
                emit_publish_progress(&app, &series_id, index + 1, total, &source_file.name);
                continue;
            }
            if !release.draft {
                return Err(format!("El release publicado {tag} ya contiene {} con otro hash; crea una versión nueva", source_file.name));
            }
            delete_release_asset(&access_token, release.id, asset.id)?;
        }
        if !release.draft {
            return Err(format!("El release publicado {tag} no contiene el mod esperado {}; crea una versión nueva", source_file.name));
        }
        let path = Path::new(&source.directory).join(&source_file.name);
        let current = fs::metadata(&path)
            .map_err(|error| format!("No se pudo inspeccionar {} para subirlo: {error}", source_file.name))?;
        if current.len() != source_file.size_bytes || sha256_file(&path)? != source_file.sha256 {
            return Err(format!("{} cambió después de la revisión; vuelve a escanear la carpeta", source_file.name));
        }
        let uploaded = upload_release_asset_resumable(
            &access_token,
            &release.upload_url,
            release.id,
            &source_file.name,
            &expected_digest,
            &path,
            source_file.size_bytes,
        )?;
        if uploaded.name != source_file.name || uploaded.digest.as_deref() != Some(expected_digest.as_str()) {
            let _ = delete_release_asset(&access_token, release.id, uploaded.id);
            return Err(format!("GitHub no confirmó el SHA-256 esperado para {}", source_file.name));
        }
        verified_names.insert(source_file.name.clone());
        emit_publish_progress(&app, &series_id, index + 1, total, &source_file.name);
    }
    if release.draft {
        for asset in &assets {
            if !verified_names.contains(&asset.name) {
                delete_release_asset(&access_token, release.id, asset.id)?;
            }
        }
    }

    let manifest = Manifest {
        schema_version: 1,
        series_id: series_id.clone(),
        version: version.clone(),
        minecraft_version: series.minecraft_version.clone(),
        loader: series.loader.clone(),
        loader_version: series.loader_version.clone(),
        files: source
            .files
            .iter()
            .map(|file| ManifestFile {
                path: format!("mods/{}", file.name),
                url: format!("https://github.com/{PUBLISH_REPOSITORY}/releases/download/{tag}/{}", encode_path_component(&file.name)),
                size_bytes: file.size_bytes,
                sha256: file.sha256.clone(),
            })
            .collect(),
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest)
        .map_err(|error| format!("No se pudo generar el manifiesto: {error}"))?;
    if release.draft {
        publish_github_release(&access_token, release.id, &series.name, &version)?;
    }

    // Publish the release assets before advancing the manifest pointer. Existing clients
    // keep resolving the previous complete version while the new assets are not yet public.
    let manifest_path = format!("packs/{series_id}/manifest.json");
    let current_manifest = get_repository_file(&access_token, &manifest_path)?
        .map(|file| file.content.ok_or_else(|| format!("GitHub no devolvió el manifiesto {manifest_path}")))
        .transpose()?
        .map(|content| base64_decode(&content))
        .transpose()?;
    if should_advance_manifest_pointer(current_manifest.as_deref(), &manifest_bytes)? {
        put_repository_file(&access_token, &manifest_path, &manifest_bytes, &format!("Publish {} {version} manifest", series.name))?;
    }
    update_catalog_status(&access_token, &series_id)?;
    super::mark_runtime_series_available(&series_id)?;
    emit_publish_progress(&app, &series_id, total, total, "Publicación verificada");
    Ok(format!("Release {tag} publicado: {total} mods, {}. La serie ya está disponible para el launcher.", format_bytes(source.total_bytes)))
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

fn emit_publish_progress(app: &AppHandle, series_id: &str, completed: usize, total: usize, message: &str) {
    let _ = app.emit("pack-publish-progress", PackPublishProgress {
        series_id: series_id.to_string(),
        completed_files: completed,
        total_files: total,
        message: message.to_string(),
    });
}

fn published_asset_names_match(source: &[PackSourceFile], assets: &[GitHubReleaseAsset]) -> bool {
    let source_names: BTreeSet<&str> = source.iter().map(|file| file.name.as_str()).collect();
    let asset_names: BTreeSet<&str> = assets.iter().map(|asset| asset.name.as_str()).collect();
    source_names == asset_names
}

fn should_advance_manifest_pointer(existing: Option<&[u8]>, expected: &[u8]) -> Result<bool, String> {
    let expected: serde_json::Value = serde_json::from_slice(expected)
        .map_err(|error| format!("Manifiesto esperado inválido: {error}"))?;
    let target_version = expected["version"].as_str()
        .ok_or_else(|| "El manifiesto esperado no contiene versión".to_string())?;
    let target = parse_pack_version(target_version)
        .ok_or_else(|| "La versión del manifiesto esperado no es SemVer estable".to_string())?;
    let Some(existing) = existing else {
        return Ok(true);
    };
    let existing: serde_json::Value = serde_json::from_slice(existing)
        .map_err(|error| format!("El manifiesto actual no es JSON válido: {error}"))?;
    if existing == expected {
        return Ok(false);
    }
    let current_version = existing["version"].as_str()
        .ok_or_else(|| "El manifiesto actual no contiene versión; no se reemplazó".to_string())?;
    let current = parse_pack_version(current_version)
        .ok_or_else(|| "El manifiesto actual no tiene una versión estable válida; no se reemplazó".to_string())?;
    match target.cmp(&current) {
        std::cmp::Ordering::Greater => Ok(true),
        std::cmp::Ordering::Equal => Err(format!(
            "La versión {target_version} ya tiene un manifiesto distinto; usa una versión nueva"
        )),
        std::cmp::Ordering::Less => Err(format!(
            "La versión {target_version} es anterior al manifiesto actual {current_version}; no se permite retroceder"
        )),
    }
}

fn parse_pack_version(version: &str) -> Option<(u32, u32, u32)> {
    if !valid_pack_version(version) {
        return None;
    }
    let mut parts = version.split('.').map(str::parse::<u32>);
    Some((parts.next()?.ok()?, parts.next()?.ok()?, parts.next()?.ok()?))
}

fn github_json<T: for<'de> Deserialize<'de>>(response: reqwest::blocking::Response, context: &str) -> Result<T, String> {
    let status = response.status();
    if !status.is_success() {
        let body = response.text().unwrap_or_default();
        return Err(format!("{context} falló ({status}): {}", body.chars().take(500).collect::<String>()));
    }
    response.json().map_err(|error| format!("{context}: respuesta inválida de GitHub: {error}"))
}

fn get_or_create_draft_release(token: &str, tag: &str, series_name: &str) -> Result<GitHubRelease, String> {
    let client = http::client().map_err(|error| error.to_string())?;
    let response = client.get(format!("{GITHUB_API}/repos/{PUBLISH_REPOSITORY}/releases/tags/{tag}"))
        .bearer_auth(token).header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28").header("User-Agent", "EternalCraft-Launcher")
        .send().map_err(|error| format!("No se pudo consultar el release {tag}: {error}"))?;
    if response.status().is_success() {
        let release: GitHubRelease = github_json(response, "Consultar release")?;
        validate_upload_url(&release.upload_url, release.id)?;
        return Ok(release);
    }
    if response.status().as_u16() != 404 {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        return Err(format!("No se pudo consultar el release ({status}): {}", body.chars().take(400).collect::<String>()));
    }
    let response = client.post(format!("{GITHUB_API}/repos/{PUBLISH_REPOSITORY}/releases"))
        .bearer_auth(token).header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28").header("User-Agent", "EternalCraft-Launcher")
        .json(&serde_json::json!({"tag_name": tag, "name": format!("{series_name} {tag}"), "body": "Publicado por EternalCraft Launcher Developer.", "draft": true, "prerelease": false}))
        .send().map_err(|error| format!("No se pudo crear el borrador {tag}: {error}"))?;
    let release: GitHubRelease = github_json(response, "Crear release borrador")?;
    validate_upload_url(&release.upload_url, release.id)?;
    Ok(release)
}

fn validate_upload_url(url: &str, release_id: u64) -> Result<(), String> {
    let expected = format!("https://uploads.github.com/repos/{PUBLISH_REPOSITORY}/releases/{release_id}/assets");
    let suffix = url.strip_prefix(&expected).unwrap_or_default();
    if suffix != "{?name,label}" && !suffix.is_empty() {
        return Err("GitHub devolvió una URL de carga inesperada; se canceló por seguridad".into());
    }
    if !url.starts_with(&expected) { return Err("GitHub devolvió una URL de carga inesperada; se canceló por seguridad".into()); }
    Ok(())
}

fn list_release_assets(token: &str, release_id: u64) -> Result<Vec<GitHubReleaseAsset>, String> {
    let client = http::client().map_err(|error| error.to_string())?;
    let mut assets = Vec::new();
    for page in 1..=10 {
        let response = client.get(format!("{GITHUB_API}/repos/{PUBLISH_REPOSITORY}/releases/{release_id}/assets?per_page=100&page={page}"))
            .bearer_auth(token).header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28").header("User-Agent", "EternalCraft-Launcher")
            .send().map_err(|error| format!("No se pudieron consultar los archivos del release: {error}"))?;
        let page_assets: Vec<GitHubReleaseAsset> = github_json(response, "Consultar assets del release")?;
        let count = page_assets.len();
        assets.extend(page_assets);
        if count < 100 { return Ok(assets); }
    }
    Err("El release excede el límite de 1000 assets soportado".into())
}

fn delete_release_asset(token: &str, release_id: u64, asset_id: u64) -> Result<(), String> {
    let client = http::client().map_err(|error| error.to_string())?;
    let response = client.delete(format!("{GITHUB_API}/repos/{PUBLISH_REPOSITORY}/releases/assets/{asset_id}"))
        .bearer_auth(token).header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28").header("User-Agent", "EternalCraft-Launcher")
        .send().map_err(|error| format!("No se pudo reemplazar un asset del release {release_id}: {error}"))?;
    if response.status().is_success() { Ok(()) } else { Err(format!("GitHub rechazó retirar el asset {asset_id} ({})", response.status())) }
}

struct AssetUploadFailure {
    message: String,
    retryable: bool,
}

fn retryable_upload_status(status: reqwest::StatusCode) -> bool {
    status.is_server_error() || status == reqwest::StatusCode::TOO_MANY_REQUESTS
}

fn upload_release_asset_once(
    token: &str,
    upload_url: &str,
    name: &str,
    path: &Path,
    size: u64,
) -> Result<GitHubReleaseAsset, AssetUploadFailure> {
    let url = format!(
        "{}?name={}",
        upload_url.split('{').next().unwrap_or(upload_url),
        encode_path_component(name)
    );
    let file = fs::File::open(path).map_err(|error| AssetUploadFailure {
        message: format!("No se pudo abrir {name} para subirlo: {error}"),
        retryable: false,
    })?;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20 * 60))
        .user_agent("EternalCraft-Launcher")
        .build()
        .map_err(|error| AssetUploadFailure {
            message: format!("No se pudo preparar la subida de {name}: {error}"),
            retryable: false,
        })?;
    let response = client
        .put(url)
        .bearer_auth(token)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .header("User-Agent", "EternalCraft-Launcher")
        .header("Content-Type", "application/java-archive")
        .header("Content-Length", size.to_string())
        .body(reqwest::blocking::Body::new(file))
        .send()
        .map_err(|error| AssetUploadFailure {
            message: format!("No se pudo subir {name}: {error}"),
            retryable: error.is_timeout() || error.is_connect(),
        })?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().unwrap_or_default();
        return Err(AssetUploadFailure {
            message: format!(
                "Subir {name} falló ({status}): {}",
                body.chars().take(500).collect::<String>()
            ),
            retryable: retryable_upload_status(status),
        });
    }
    response.json().map_err(|error| AssetUploadFailure {
        message: format!("GitHub devolvió una respuesta inválida al subir {name}: {error}"),
        retryable: true,
    })
}

fn upload_release_asset_resumable(
    token: &str,
    upload_url: &str,
    release_id: u64,
    name: &str,
    expected_digest: &str,
    path: &Path,
    size: u64,
) -> Result<GitHubReleaseAsset, String> {
    const MAX_ATTEMPTS: usize = 3;
    let mut last_error = String::new();
    for attempt in 1..=MAX_ATTEMPTS {
        match upload_release_asset_once(token, upload_url, name, path, size) {
            Ok(asset)
                if asset.name == name && asset.digest.as_deref() == Some(expected_digest) =>
            {
                return Ok(asset);
            }
            Ok(asset) => {
                let _ = delete_release_asset(token, release_id, asset.id);
                return Err(format!("GitHub no confirmó el SHA-256 esperado para {name}"));
            }
            Err(failure) => {
                last_error = failure.message;
                match list_release_assets(token, release_id) {
                    Ok(assets) => {
                        if let Some(asset) = assets.iter().find(|asset| {
                            asset.name == name && asset.digest.as_deref() == Some(expected_digest)
                        }) {
                            return Ok(GitHubReleaseAsset {
                                id: asset.id,
                                name: asset.name.clone(),
                                digest: asset.digest.clone(),
                            });
                        }
                        let mut removed_stale_asset = false;
                        for asset in assets.iter().filter(|asset| asset.name == name) {
                            delete_release_asset(token, release_id, asset.id)?;
                            removed_stale_asset = true;
                        }
                        if !failure.retryable && !removed_stale_asset {
                            return Err(last_error);
                        }
                        if attempt == MAX_ATTEMPTS {
                            break;
                        }
                    }
                    Err(verify_error) if !failure.retryable || attempt == MAX_ATTEMPTS => {
                        return Err(format!(
                            "{last_error}. No se pudo confirmar si GitHub aceptó el archivo: {verify_error}"
                        ));
                    }
                    Err(_) => {}
                }
                if attempt < MAX_ATTEMPTS {
                    std::thread::sleep(Duration::from_secs(2_u64.pow(attempt as u32)));
                }
            }
        }
    }
    Err(format!(
        "No se pudo confirmar la carga de {name} después de {MAX_ATTEMPTS} intentos: {last_error}"
    ))
}

fn publish_github_release(token: &str, release_id: u64, series_name: &str, version: &str) -> Result<(), String> {
    let client = http::client().map_err(|error| error.to_string())?;
    let response = client.patch(format!("{GITHUB_API}/repos/{PUBLISH_REPOSITORY}/releases/{release_id}"))
        .bearer_auth(token).header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28").header("User-Agent", "EternalCraft-Launcher")
        .json(&serde_json::json!({"draft": false, "name": format!("{series_name} {version}"), "body": "Versión oficial publicada por EternalCraft Launcher Developer."}))
        .send().map_err(|error| format!("No se pudo publicar el release: {error}"))?;
    let release: GitHubRelease = github_json(response, "Publicar release")?;
    if release.draft { return Err("GitHub no confirmó la publicación del release".into()); }
    Ok(())
}

fn put_repository_file(token: &str, path: &str, bytes: &[u8], message: &str) -> Result<(), String> {
    let endpoint = format!("{GITHUB_API}/repos/{PUBLISH_REPOSITORY}/contents/{path}");
    let current = get_repository_file(token, path)?;
    let sha = current.map(|content| content.sha);
    let client = http::client().map_err(|error| error.to_string())?;
    let mut body = serde_json::json!({"message": message, "content": base64_encode(bytes)});
    if let Some(sha) = sha { body["sha"] = serde_json::Value::String(sha); }
    let response = client.put(endpoint).bearer_auth(token).header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28").header("User-Agent", "EternalCraft-Launcher")
        .json(&body).send().map_err(|error| format!("No se pudo guardar {path} en GitHub: {error}"))?;
    if response.status().is_success() { Ok(()) }
    else { Err(format!("GitHub rechazó actualizar {path} ({}): {}", response.status(), response.text().unwrap_or_default().chars().take(400).collect::<String>())) }
}

fn get_repository_file(token: &str, path: &str) -> Result<Option<GitHubContentResponse>, String> {
    let client = http::client().map_err(|error| error.to_string())?;
    let response = client.get(format!("{GITHUB_API}/repos/{PUBLISH_REPOSITORY}/contents/{path}"))
        .bearer_auth(token).header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28").header("User-Agent", "EternalCraft-Launcher")
        .send().map_err(|error| format!("No se pudo leer {path} en GitHub: {error}"))?;
    if response.status().is_success() {
        github_json(response, &format!("Leer {path}")).map(Some)
    } else if response.status().as_u16() == 404 {
        Ok(None)
    } else {
        Err(format!("GitHub no pudo leer {path} ({})", response.status()))
    }
}

fn update_catalog_status(token: &str, series_id: &str) -> Result<(), String> {
    let path = "resources/series/catalog.json";
    let client = http::client().map_err(|error| error.to_string())?;
    let response = client.get(format!("{GITHUB_API}/repos/{PUBLISH_REPOSITORY}/contents/{path}"))
        .bearer_auth(token).header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", "2022-11-28").header("User-Agent", "EternalCraft-Launcher")
        .send().map_err(|error| format!("No se pudo leer el catálogo remoto: {error}"))?;
    let encoded: GitHubContentResponse = github_json(response, "Cargar catálogo remoto")?;
    let decoded = base64_decode(encoded.content.as_deref().ok_or_else(|| "GitHub no devolvió el catálogo codificado".to_string())?)?;
    let mut catalog: serde_json::Value = serde_json::from_slice(&decoded).map_err(|error| format!("El catálogo remoto no es JSON válido: {error}"))?;
    let series = catalog["series"].as_array_mut().ok_or_else(|| "El catálogo remoto no tiene una lista de series".to_string())?;
    let entry = series.iter_mut().find(|entry| entry["id"].as_str() == Some(series_id))
        .ok_or_else(|| format!("La serie {series_id} ya no existe en el catálogo remoto"))?;
    entry["packStatus"] = serde_json::Value::String("available".into());
    let bytes = serde_json::to_vec_pretty(&catalog).map_err(|error| error.to_string())?;
    put_repository_file(token, path, &bytes, &format!("Enable {series_id} official pack"))?;
    Ok(())
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

fn base64_encode(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let a = chunk[0];
        let b = *chunk.get(1).unwrap_or(&0);
        let c = *chunk.get(2).unwrap_or(&0);
        output.push(TABLE[(a >> 2) as usize] as char);
        output.push(TABLE[(((a & 3) << 4) | (b >> 4)) as usize] as char);
        output.push(if chunk.len() > 1 { TABLE[(((b & 15) << 2) | (c >> 6)) as usize] as char } else { '=' });
        output.push(if chunk.len() > 2 { TABLE[(c & 63) as usize] as char } else { '=' });
    }
    output
}

fn base64_decode(encoded: &str) -> Result<Vec<u8>, String> {
    let bytes: Vec<u8> = encoded.bytes().filter(|byte| !byte.is_ascii_whitespace()).collect();
    if bytes.len() % 4 != 0 { return Err("GitHub devolvió base64 incompleto".into()); }
    let value = |byte: u8| -> Option<u8> {
        match byte { b'A'..=b'Z' => Some(byte - b'A'), b'a'..=b'z' => Some(byte - b'a' + 26), b'0'..=b'9' => Some(byte - b'0' + 52), b'+' => Some(62), b'/' => Some(63), _ => None }
    };
    let mut output = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks_exact(4) {
        let a = value(chunk[0]).ok_or_else(|| "GitHub devolvió base64 inválido".to_string())?;
        let b = value(chunk[1]).ok_or_else(|| "GitHub devolvió base64 inválido".to_string())?;
        let c = if chunk[2] == b'=' { 0 } else { value(chunk[2]).ok_or_else(|| "GitHub devolvió base64 inválido".to_string())? };
        let d = if chunk[3] == b'=' { 0 } else { value(chunk[3]).ok_or_else(|| "GitHub devolvió base64 inválido".to_string())? };
        output.push((a << 2) | (b >> 4));
        if chunk[2] != b'=' { output.push((b << 4) | (c >> 2)); }
        if chunk[3] != b'=' { output.push((c << 6) | d); }
    }
    Ok(output)
}

fn format_bytes(bytes: u64) -> String {
    if bytes < 1024 * 1024 { format!("{} KiB", bytes / 1024) }
    else { format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0)) }
}

fn scan_pack_source(path: &Path, series_id: &str) -> Result<PackSourcePreview, String> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| format!("No se pudo inspeccionar la carpeta elegida: {error}"))?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err("La fuente debe ser una carpeta real, no un enlace simbólico".into());
    }
    let directory = path
        .canonicalize()
        .map_err(|error| format!("No se pudo resolver la carpeta fuente: {error}"))?;
    if directory
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("personales"))
    {
        return Err("No se puede publicar una carpeta llamada personales como pack oficial".into());
    }

    let mut files = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut total_bytes = 0_u64;
    for entry in fs::read_dir(&directory)
        .map_err(|error| format!("No se pudo leer la carpeta fuente: {error}"))?
    {
        let entry = entry.map_err(|error| format!("No se pudo leer un elemento de la fuente: {error}"))?;
        let file_type = entry
            .file_type()
            .map_err(|error| format!("No se pudo inspeccionar un elemento de la fuente: {error}"))?;
        if !file_type.is_file() {
            continue;
        }
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else { continue };
        if !name.to_ascii_lowercase().ends_with(".jar") {
            continue;
        }
        if !valid_official_asset_name(name) {
            return Err(format!("El nombre de archivo no es compatible con Forge/GitHub: {name}"));
        }
        if !seen.insert(name.to_ascii_lowercase()) {
            return Err(format!("Hay nombres de mod duplicados sin distinguir mayúsculas: {name}"));
        }
        let jar_path = entry.path();
        let before = entry
            .metadata()
            .map_err(|error| format!("No se pudo leer el tamaño de {name}: {error}"))?;
        if before.len() == 0 || before.len() > MAX_SOURCE_JAR_BYTES {
            return Err(format!("El mod {name} está vacío o supera 256 MiB"));
        }
        total_bytes = total_bytes
            .checked_add(before.len())
            .ok_or_else(|| "El tamaño total de los mods excede 4 GiB".to_string())?;
        if total_bytes > MAX_SOURCE_PACK_BYTES {
            return Err("El tamaño total de los mods supera 4 GiB".into());
        }
        validate_forge_mod_archive(&jar_path)
            .map_err(|error| format!("{name}: {error}"))?;
        let license = read_pack_license(&jar_path);
        let license_status = pack_license_status(license.as_deref());
        let digest = sha256_file(&jar_path)?;
        let after = fs::metadata(&jar_path)
            .map_err(|error| format!("No se pudo verificar el origen de {name}: {error}"))?;
        if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
            return Err(format!("El archivo {name} cambió mientras se verificaba"));
        }
        files.push(PackSourceFile {
            name: name.to_string(),
            size_bytes: before.len(),
            sha256: digest,
            license,
            license_status,
        });
    }
    files.sort_by(|a, b| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()));
    if files.is_empty() {
        return Err("La carpeta elegida no contiene archivos JAR oficiales en su nivel raíz".into());
    }
    let source_fingerprint = pack_source_fingerprint(&files);
    Ok(PackSourcePreview {
        series_id: series_id.to_string(),
        directory: directory.to_string_lossy().into_owned(),
        files,
        total_bytes,
        source_fingerprint,
    })
}

fn pack_source_fingerprint(files: &[PackSourceFile]) -> String {
    let mut hasher = sha2::Sha256::new();
    let mut sorted_files: Vec<&PackSourceFile> = files.iter().collect();
    sorted_files.sort_by(|left, right| {
        left.name
            .to_ascii_lowercase()
            .cmp(&right.name.to_ascii_lowercase())
    });
    for file in sorted_files {
        hasher.update(file.name.to_ascii_lowercase().as_bytes());
        hasher.update([0]);
        hasher.update(file.sha256.as_bytes());
        hasher.update([0]);
    }
    format!("{:x}", hasher.finalize())
}

fn read_pack_license(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut archive = zip::ZipArchive::new(file).ok()?;
    let index = archive
        .file_names()
        .position(|name| name.eq_ignore_ascii_case("META-INF/mods.toml"))?;
    let mut metadata = archive.by_index(index).ok()?;
    let mut contents = String::new();
    metadata
        .by_ref()
        .take(256 * 1024)
        .read_to_string(&mut contents)
        .ok()?;
    let document: toml::Value = toml::from_str(&contents).ok()?;
    document
        .get("license")
        .and_then(toml::Value::as_str)
        .map(str::trim)
        .filter(|license| !license.is_empty())
        .map(str::to_string)
}

fn pack_license_status(license: Option<&str>) -> PackLicenseStatus {
    let Some(license) = license.map(str::trim).filter(|value| !value.is_empty()) else {
        return PackLicenseStatus::Unknown;
    };
    let normalized = license.to_ascii_lowercase();
    let words: Vec<&str> = normalized
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect();
    let has_arr_marker = words.contains(&"arr");
    if normalized.contains("all rights reserved")
        || normalized.replace('-', " ").contains("all rights reserved")
        || normalized.contains("noncommercial")
        || normalized.contains("non-commercial")
        || normalized.contains("by-nc")
        || normalized.contains("by nc")
        || normalized.contains("protective")
        || has_arr_marker
    {
        return PackLicenseStatus::PermissionRequired;
    }
    let recognized = words.contains(&"mit")
        || words.contains(&"apache")
        || words.contains(&"bsd")
        || words.iter().any(|word| word.starts_with("lgpl"))
        || words.iter().any(|word| word.starts_with("gpl"))
        || words.iter().any(|word| word.starts_with("mpl"))
        || words.contains(&"unlicense")
        || words.contains(&"academic") && words.contains(&"license")
        || words.contains(&"lesser")
            && words.contains(&"general")
            && words.contains(&"public")
            && words.contains(&"license")
        || words.contains(&"general")
            && words.contains(&"public")
            && words.contains(&"license");
    if recognized {
        PackLicenseStatus::Recognized
    } else {
        PackLicenseStatus::Unknown
    }
}

fn valid_official_asset_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 180
        && name.to_ascii_lowercase().ends_with(".jar")
        && !name.starts_with('.')
        && !name.starts_with(' ')
        && !name.ends_with(' ')
        && !name.ends_with('.')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b" ._+-()[]'".contains(&byte))
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = fs::File::open(path)
        .map_err(|error| format!("No se pudo abrir {} para calcular SHA-256: {error}", path.display()))?;
    let mut hasher = sha2::Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("No se pudo calcular SHA-256 de {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_forge_jar(path: &Path) {
        use std::io::Write;
        let file = fs::File::create(path).unwrap();
        let mut archive = zip::ZipWriter::new(file);
        archive
            .start_file("META-INF/mods.toml", zip::write::SimpleFileOptions::default())
            .unwrap();
        archive
            .write_all(b"modLoader=\"javafml\"\nlicense=\"MIT\"\n")
            .unwrap();
        archive.finish().unwrap();
    }

    fn temporary_directory(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "eternalcraft-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn concurrent_pack_publications_are_serialized_for_catalog_consistency() {
        let mut active = false;
        assert!(reserve_publish(&mut active));
        assert!(!reserve_publish(&mut active));
        active = false;
        assert!(reserve_publish(&mut active));
    }

    #[test]
    fn published_release_asset_set_must_match_the_source_exactly() {
        let source = vec![
            PackSourceFile {
                name: "alpha.jar".into(),
                size_bytes: 10,
                sha256: "a".repeat(64),
                license: Some("MIT".into()),
                license_status: PackLicenseStatus::Recognized,
            },
            PackSourceFile {
                name: "beta.jar".into(),
                size_bytes: 20,
                sha256: "b".repeat(64),
                license: Some("MIT".into()),
                license_status: PackLicenseStatus::Recognized,
            },
        ];
        let complete = vec![
            GitHubReleaseAsset { id: 2, name: "beta.jar".into(), digest: None },
            GitHubReleaseAsset { id: 1, name: "alpha.jar".into(), digest: None },
        ];
        assert!(published_asset_names_match(&source, &complete));
        assert!(!published_asset_names_match(&source, &complete[..1]));
        let with_stale_asset = [complete.as_slice(), &[GitHubReleaseAsset { id: 3, name: "retired.jar".into(), digest: None }]].concat();
        assert!(!published_asset_names_match(&source, &with_stale_asset));
    }

    #[test]
    fn pack_source_fingerprint_changes_when_a_mod_changes_and_is_order_independent() {
        let first = PackSourceFile {
            name: "Alpha.jar".into(),
            size_bytes: 10,
            sha256: "a".repeat(64),
            license: Some("MIT".into()),
            license_status: PackLicenseStatus::Recognized,
        };
        let second = PackSourceFile {
            name: "beta.jar".into(),
            size_bytes: 20,
            sha256: "b".repeat(64),
            license: Some("MIT".into()),
            license_status: PackLicenseStatus::Recognized,
        };
        let original = pack_source_fingerprint(&[first.clone(), second.clone()]);
        assert_eq!(original.len(), 64);
        assert_eq!(original, pack_source_fingerprint(&[second.clone(), first.clone()]));
        let changed = PackSourceFile {
            sha256: "c".repeat(64),
            ..first
        };
        assert_ne!(original, pack_source_fingerprint(&[changed, second]));
    }

    #[test]
    fn manifest_pointer_only_advances_after_a_new_complete_release() {
        let expected = br#"{"seriesId":"siege","version":"1.2.0","files":[{"path":"mods/a.jar","sha256":"abc"}]}"#;
        let same = br#"{
            "seriesId": "siege",
            "version": "1.2.0",
            "files": [{"path": "mods/a.jar", "sha256": "abc"}]
        }"#;
        let same_version_changed = br#"{"seriesId":"siege","version":"1.2.0","files":[]}"#;
        let older = br#"{"seriesId":"siege","version":"1.1.9","files":[]}"#;
        let newer = br#"{"seriesId":"siege","version":"1.3.0","files":[]}"#;
        assert!(should_advance_manifest_pointer(None, expected).unwrap());
        assert!(!should_advance_manifest_pointer(Some(same), expected).unwrap());
        assert!(should_advance_manifest_pointer(Some(older), expected).unwrap());
        assert!(should_advance_manifest_pointer(Some(same_version_changed), expected).is_err());
        assert!(should_advance_manifest_pointer(Some(newer), expected).is_err());
        assert!(should_advance_manifest_pointer(Some(b"not json"), expected).is_err());
    }

    #[test]
    fn pack_versions_compare_numerically() {
        assert_eq!(parse_pack_version("1.10.0"), Some((1, 10, 0)));
        assert!(parse_pack_version("1.2.3-beta").is_none());
    }

    #[test]
    fn pack_license_review_flags_restricted_and_unknown_metadata() {
        assert_eq!(pack_license_status(Some("MIT")), PackLicenseStatus::Recognized);
        assert_eq!(pack_license_status(Some("LGPL-3.0")), PackLicenseStatus::Recognized);
        assert_eq!(pack_license_status(Some("All Rights Reserved")), PackLicenseStatus::PermissionRequired);
        assert_eq!(pack_license_status(Some("All-Rights-Reserved")), PackLicenseStatus::PermissionRequired);
        assert_eq!(
            pack_license_status(Some("MIT License, Art Resources: All Rights Reserved.")),
            PackLicenseStatus::PermissionRequired
        );
        assert_eq!(pack_license_status(Some("ARR")), PackLicenseStatus::PermissionRequired);
        assert_eq!(pack_license_status(Some("CC BY-NC-ND 4.0")), PackLicenseStatus::PermissionRequired);
        assert_eq!(pack_license_status(Some("Not specified")), PackLicenseStatus::Unknown);
        assert_eq!(pack_license_status(Some("AGNYA License")), PackLicenseStatus::Unknown);
        assert_eq!(pack_license_status(None), PackLicenseStatus::Unknown);
    }

    #[test]
    fn release_upload_retry_policy_retries_throttling_and_server_errors_only() {
        assert!(retryable_upload_status(reqwest::StatusCode::TOO_MANY_REQUESTS));
        assert!(retryable_upload_status(reqwest::StatusCode::BAD_GATEWAY));
        assert!(retryable_upload_status(reqwest::StatusCode::INTERNAL_SERVER_ERROR));
        assert!(!retryable_upload_status(reqwest::StatusCode::FORBIDDEN));
        assert!(!retryable_upload_status(reqwest::StatusCode::UNPROCESSABLE_ENTITY));
    }

    #[test]
    fn github_developer_access_requires_contents_write_permission() {
        let mut permissions = BTreeMap::new();
        permissions.insert("contents".into(), "write".into());
        assert!(installation_has_contents_write(&permissions));
        permissions.insert("contents".into(), "read".into());
        assert!(!installation_has_contents_write(&permissions));
        assert!(!installation_has_contents_write(&BTreeMap::new()));
    }

    #[test]
    fn github_developer_access_requires_user_write_on_installed_repository() {
        assert!(repository_permissions_allow_write(Some(RepositoryPermissions {
            push: Some(true),
            admin: Some(false),
        })));
        assert!(repository_permissions_allow_write(Some(RepositoryPermissions {
            push: Some(false),
            admin: Some(true),
        })));
        assert!(!repository_permissions_allow_write(Some(RepositoryPermissions {
            push: Some(false),
            admin: Some(false),
        })));
        assert!(!repository_permissions_allow_write(None));
    }

    #[test]
    fn github_app_client_id_validation_rejects_urls_and_empty_values() {
        assert!(valid_github_app_client_id("Iv1.abcDEF_123-456"));
        assert!(!valid_github_app_client_id(""));
        assert!(!valid_github_app_client_id("https://github.com"));
    }

    #[test]
    fn official_asset_names_are_flat_jar_names_without_path_or_shell_characters() {
        assert!(valid_official_asset_name("Example Mod-1.2.3.jar"));
        assert!(!valid_official_asset_name("../Example.jar"));
        assert!(!valid_official_asset_name("subdir/Example.jar"));
        assert!(!valid_official_asset_name("Example.jar "));
        assert!(!valid_official_asset_name("Example.jar;rm -rf"));
        assert!(!valid_official_asset_name(".hidden.jar"));
    }

    #[test]
    fn pack_versions_are_stable_three_part_numbers() {
        assert!(valid_pack_version("1.2.0"));
        assert!(valid_pack_version("0.0.1"));
        assert!(!valid_pack_version("1.2"));
        assert!(!valid_pack_version("01.2.3"));
        assert!(!valid_pack_version("1.2.3-beta"));
    }

    #[test]
    fn base64_content_and_release_asset_names_round_trip() {
        let original = b"{series: ghouls outbreak}";
        assert_eq!(base64_decode(&base64_encode(original)).unwrap(), original);
        assert_eq!(encode_path_component("Mod Name (1).jar"), "Mod%20Name%20%281%29.jar");
    }

    #[test]
    fn pack_source_publishes_only_root_forge_jars_and_ignores_personal_and_config_files() {
        let source = temporary_directory("developer-pack-source");
        fs::create_dir_all(source.join("personales")).unwrap();
        fs::create_dir_all(source.join("config")).unwrap();
        write_forge_jar(&source.join("Official Mod.jar"));
        write_forge_jar(&source.join("personales/Personal Mod.jar"));
        fs::write(source.join("config/options.txt"), b"user settings").unwrap();
        fs::write(source.join("options.txt"), b"user settings").unwrap();

        let preview = scan_pack_source(&source, "siege").unwrap();
        assert_eq!(preview.files.len(), 1);
        assert_eq!(preview.files[0].name, "Official Mod.jar");
        assert_eq!(preview.files[0].license.as_deref(), Some("MIT"));
        assert_eq!(preview.files[0].license_status, PackLicenseStatus::Recognized);
        assert_eq!(preview.source_fingerprint.len(), 64);
        assert_eq!(preview.files[0].sha256, sha256_file(&source.join("Official Mod.jar")).unwrap());
        assert_eq!(preview.total_bytes, preview.files[0].size_bytes);

        fs::remove_dir_all(source).unwrap();
    }

    #[test]
    fn pack_source_refuses_to_publish_the_personal_mods_directory_as_official() {
        let source = temporary_directory("developer-personal-source");
        write_forge_jar(&source.join("Personal Mod.jar"));
        let personal = source.join("Personales");
        fs::create_dir(&personal).unwrap();

        assert!(scan_pack_source(&personal, "siege").is_err());
        fs::remove_dir_all(source).unwrap();
    }
}
