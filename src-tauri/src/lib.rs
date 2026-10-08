use atomic_write_file::AtomicWriteFile;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, io::Write, path::{Path, PathBuf}};
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
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Bootstrap {
    series: Vec<Series>,
    active_series_id: String,
    game_directories: BTreeMap<String, String>,
    suggested_directories: BTreeMap<String, String>,
    config_directory: String,
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
    let dir = app.path().app_config_dir().map_err(|error| error.to_string())?;
    fs::create_dir_all(&dir).map_err(|error| format!("No se pudo crear la configuración local: {error}"))?;
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
    file.write_all(&bytes).map_err(|error| format!("No se pudo escribir la configuración: {error}"))?;
    file.commit().map_err(|error| format!("No se pudo confirmar la configuración: {error}"))
}

fn valid_game_dir(path: &Path) -> Result<PathBuf, String> {
    if !path.is_dir() {
        return Err("La carpeta seleccionada ya no existe o no es un directorio".into());
    }
    path.canonicalize().map_err(|error| format!("No se pudo resolver la carpeta seleccionada: {error}"))
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

fn make_bootstrap(app: &AppHandle) -> Result<Bootstrap, String> {
    let catalog = parse_catalog()?;
    let settings = read_settings(app)?;
    let default_series = catalog.series.first().expect("validated non-empty catalog").id.clone();
    let active = settings.active_series_id
        .filter(|id| catalog.series.iter().any(|series| &series.id == id))
        .unwrap_or(default_series);
    let valid_ids: std::collections::BTreeSet<&str> = catalog.series.iter().map(|series| series.id.as_str()).collect();
    let directories = settings.game_directories.into_iter()
        .filter(|(id, path)| valid_ids.contains(id.as_str()) && Path::new(path).is_dir())
        .collect();
    let config_directory = settings_path(app)?.parent().unwrap().to_string_lossy().into_owned();
    Ok(Bootstrap {
        series: catalog.series,
        active_series_id: active,
        game_directories: directories,
        suggested_directories: suggested_directories(),
        config_directory,
    })
}

#[tauri::command]
fn get_bootstrap(app: AppHandle) -> Result<Bootstrap, String> {
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
    let selected = app.dialog().file().set_title("Seleccionar carpeta de juego").blocking_pick_folder();
    let Some(selected) = selected else { return make_bootstrap(&app); };
    let chosen = selected.into_path().map_err(|error| format!("Ruta de carpeta no válida: {error}"))?;
    let chosen = valid_game_dir(&chosen)?;
    let mut settings = read_settings(&app)?;
    settings.active_series_id = Some(series_id.clone());
    settings.game_directories.insert(series_id, chosen.to_string_lossy().into_owned());
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
    let path = suggestions.get(&series_id)
        .ok_or_else(|| "No se encontró una instancia compatible para esta serie".to_string())?;
    let path = valid_game_dir(Path::new(path))?;
    let mut settings = read_settings(&app)?;
    settings.active_series_id = Some(series_id.clone());
    settings.game_directories.insert(series_id, path.to_string_lossy().into_owned());
    write_settings(&app, &settings)?;
    make_bootstrap(&app)
}

pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .invoke_handler(tauri::generate_handler![get_bootstrap, set_active_series, select_game_directory, link_detected_directory])
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
        assert!(catalog.series.iter().any(|series| series.id == "ghouls-outbreak"));
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
        };
        let encoded = serde_json::to_vec(&settings).expect("settings serialize");
        let decoded: Settings = serde_json::from_slice(&encoded).expect("settings deserialize");
        assert_eq!(decoded.active_series_id.as_deref(), Some("ghouls-outbreak"));
        assert_eq!(decoded.game_directories, settings.game_directories);
    }
}
