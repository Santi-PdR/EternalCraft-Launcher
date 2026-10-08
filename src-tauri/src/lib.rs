use atomic_write_file::AtomicWriteFile;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    env, fs,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
};
use tauri::{AppHandle, Manager};
use tauri_plugin_dialog::DialogExt;

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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Catalog {
    schema_version: u32,
    series: Vec<Series>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct Settings {
    active_series_id: Option<String>,
    #[serde(default)]
    game_directories: BTreeMap<String, String>,
    #[serde(default)]
    java_executable: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Bootstrap {
    series: Vec<Series>,
    active_series_id: String,
    game_directories: BTreeMap<String, String>,
    suggested_directories: BTreeMap<String, String>,
    config_directory: String,
    java: JavaStatus,
    java_manually_selected: bool,
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

fn parse_catalog() -> Result<Catalog, String> {
    let catalog: Catalog = serde_json::from_str(CATALOG_JSON)
        .map_err(|error| format!("El catálogo integrado no es válido: {error}"))?;
    if catalog.schema_version != 1 || catalog.series.is_empty() {
        return Err("La versión del catálogo no es compatible o no contiene series".into());
    }
    let mut ids = std::collections::BTreeSet::new();
    for series in &catalog.series {
        if series.id.is_empty() || !ids.insert(series.id.as_str()) {
            return Err("El catálogo contiene identificadores de serie vacíos o duplicados".into());
        }
    }
    Ok(catalog)
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

fn make_bootstrap(app: &AppHandle) -> Result<Bootstrap, String> {
    let catalog = parse_catalog()?;
    let settings = read_settings(app)?;
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
    let java_manually_selected = settings.java_executable.is_some();
    let java = settings
        .java_executable
        .as_deref()
        .map(|path| inspect_java(Path::new(path)))
        .unwrap_or_else(detect_java);
    Ok(Bootstrap {
        series: catalog.series,
        active_series_id: active,
        game_directories: directories,
        suggested_directories: suggested_directories(),
        config_directory,
        java,
        java_manually_selected,
    })
}

#[tauri::command]
fn get_bootstrap(app: AppHandle) -> Result<Bootstrap, String> {
    make_bootstrap(&app)
}

#[tauri::command]
fn get_mod_inventory(app: AppHandle, series_id: String) -> Result<ModInventory, String> {
    if !parse_catalog()?
        .series
        .iter()
        .any(|series| series.id == series_id)
    {
        return Err("La serie solicitada no existe en el catálogo".into());
    }
    let settings = read_settings(&app)?;
    let Some(path) = settings.game_directories.get(&series_id) else {
        return Ok(ModInventory {
            game_directory: None,
            mods_directory: None,
            loaded_from_mods_root: Vec::new(),
            official_store: Vec::new(),
            personal_store: Vec::new(),
        });
    };
    let game_directory = valid_game_dir(Path::new(path))?;
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
fn set_active_series(app: AppHandle, series_id: String) -> Result<Bootstrap, String> {
    let catalog = parse_catalog()?;
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
    let catalog = parse_catalog()?;
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
    let catalog = parse_catalog()?;
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
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![
            get_bootstrap,
            get_mod_inventory,
            refresh_java_status,
            select_java_executable,
            reset_java_selection,
            set_active_series,
            select_game_directory,
            link_detected_directory
        ])
        .run(tauri::generate_context!())
        .expect("error while running EternalCraft Launcher");
}

#[cfg(test)]
mod tests {
    use super::*;

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
        };
        let encoded = serde_json::to_vec(&settings).expect("settings serialize");
        let decoded: Settings = serde_json::from_slice(&encoded).expect("settings deserialize");
        assert_eq!(decoded.active_series_id.as_deref(), Some("ghouls-outbreak"));
        assert_eq!(decoded.game_directories, settings.game_directories);
        assert_eq!(decoded.java_executable, settings.java_executable);
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
}
