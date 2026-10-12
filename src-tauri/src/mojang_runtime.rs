use atomic_write_file::AtomicWriteFile;
use serde::Deserialize;
use sha1::{Digest, Sha1};
use std::{
    collections::HashMap,
    env, fs,
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
use xz2::{read::XzDecoder, stream::Stream};

const JVM_MANIFEST_URL: &str = "https://launchermeta.mojang.com/v1/products/java-runtime/2ec0cc96c44e5a76b9c8b7c39df7210883d12871/all.json";
const LZMA_MEMORY_LIMIT: u64 = 256 * 1024 * 1024;

#[derive(Deserialize)]
struct RuntimeManifest {
    #[serde(flatten)]
    platforms: HashMap<String, HashMap<String, Vec<RuntimeManifestEntry>>>,
}

#[derive(Deserialize)]
struct RuntimeManifestEntry {
    manifest: RuntimeManifestFile,
    version: HashMap<String, String>,
}

#[derive(Deserialize)]
struct RuntimeManifestFile {
    url: String,
}

#[derive(Deserialize)]
struct PlatformManifest {
    files: HashMap<String, RuntimeFile>,
}

#[derive(Deserialize)]
struct RuntimeFile {
    #[serde(rename = "type")]
    file_type: Option<String>,
    downloads: Option<HashMap<String, RuntimeDownload>>,
    executable: Option<bool>,
    target: Option<String>,
}

#[derive(Deserialize)]
struct RuntimeDownload {
    sha1: String,
    url: String,
}

pub fn install_java_runtime(component: &str, minecraft_dir: &Path) -> Result<(), String> {
    let platform = runtime_platform()?;
    let component_path = safe_relative_path(component)?;
    if component_path.components().count() != 1
        || component_path.file_name().and_then(|name| name.to_str()) != Some(component)
    {
        return Err("El identificador del runtime Java no es una carpeta válida".into());
    }
    let client = reqwest::blocking::Client::builder()
        .user_agent(concat!("EternalCraft Launcher/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|error| format!("No se pudo preparar la conexión con Mojang: {error}"))?;
    let manifest: RuntimeManifest = client
        .get(JVM_MANIFEST_URL)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .and_then(|response| response.json::<RuntimeManifest>())
        .map_err(|error| format!("No se pudo consultar el catálogo Java de Mojang: {error}"))?;
    let entry = manifest
        .platforms
        .get(&platform)
        .and_then(|components| components.get(component))
        .and_then(|entries| entries.first())
        .ok_or_else(|| format!("Mojang no publica el runtime {component} para {platform}"))?;
    let platform_manifest: PlatformManifest = client
        .get(&entry.manifest.url)
        .send()
        .and_then(reqwest::blocking::Response::error_for_status)
        .and_then(|response| response.json::<PlatformManifest>())
        .map_err(|error| format!("No se pudo leer el manifiesto Java de Mojang: {error}"))?;

    let canonical_game_dir = fs::canonicalize(minecraft_dir)
        .map_err(|error| format!("No se pudo resolver el directorio de juego: {error}"))?;
    let runtime_root = canonical_game_dir
        .join("runtime")
        .join(component)
        .join(&platform);
    let install_root = runtime_root.join(component);
    for directory in [
        canonical_game_dir.join("runtime"),
        canonical_game_dir.join("runtime").join(component),
        runtime_root.clone(),
        install_root.clone(),
    ] {
        ensure_normal_directory(&directory)?;
    }
    let canonical_install_root = fs::canonicalize(&install_root)
        .map_err(|error| format!("No se pudo resolver el directorio del runtime: {error}"))?;
    if !canonical_install_root.starts_with(&canonical_game_dir) {
        return Err("El directorio del runtime Java está fuera de la carpeta de juego".into());
    }

    let mut installed_files = Vec::new();
    for (relative_name, file) in &platform_manifest.files {
        let relative = safe_relative_path(relative_name)?;
        let destination = install_root.join(&relative);
        match file.file_type.as_deref() {
            Some("directory") => {
                ensure_runtime_directory_tree(&canonical_install_root, &destination)?;
            }
            Some("file") => {
                let downloads = file.downloads.as_ref().ok_or_else(|| {
                    format!("El manifiesto Java no incluye descargas para {relative_name}")
                })?;
                let raw = downloads.get("raw").ok_or_else(|| {
                    format!("El manifiesto Java no incluye el archivo original {relative_name}")
                })?;
                let compressed = downloads.get("lzma");
                install_runtime_file(
                    &client,
                    &destination,
                    raw,
                    compressed,
                    &canonical_install_root,
                )?;
                if file.executable == Some(true) {
                    set_executable(&destination)?;
                }
                installed_files.push(relative);
            }
            Some("link") => {
                let target = file.target.as_deref().ok_or_else(|| {
                    format!("El manifiesto Java no define el destino del enlace {relative_name}")
                })?;
                validate_link_target(&relative, target)?;
                if let Some(parent) = destination.parent() {
                    ensure_runtime_directory_tree(&canonical_install_root, parent)?;
                }
                #[cfg(unix)]
                match fs::symlink_metadata(&destination) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        let existing_target = fs::read_link(&destination).map_err(|error| {
                            format!("No se pudo inspeccionar el enlace Java {relative_name}: {error}")
                        })?;
                        if existing_target != Path::new(target) {
                            fs::remove_file(&destination).map_err(|error| {
                                format!("No se pudo sustituir el enlace Java {relative_name}: {error}")
                            })?;
                            std::os::unix::fs::symlink(Path::new(target), &destination)
                                .map_err(|error| {
                                    format!("No se pudo crear el enlace Java {relative_name}: {error}")
                                })?;
                        }
                    }
                    Ok(_) => {
                        return Err(format!(
                            "La ruta del enlace Java ya existe y no es un enlace normal: {relative_name}"
                        ));
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {
                        std::os::unix::fs::symlink(Path::new(target), &destination).map_err(
                            |error| {
                                format!("No se pudo crear el enlace Java {relative_name}: {error}")
                            },
                        )?;
                    }
                    Err(error) => {
                        return Err(format!(
                            "No se pudo inspeccionar la ruta del enlace Java {relative_name}: {error}"
                        ));
                    }
                }
                #[cfg(not(unix))]
                return Err(format!(
                    "El runtime Java requiere un enlace simbólico no disponible en esta plataforma: {relative_name}"
                ));
            }
            Some(other) => return Err(format!("Tipo de archivo Java no reconocido: {other}")),
            None => {}
        }
    }

    for (relative_name, file) in &platform_manifest.files {
        if file.file_type.as_deref() == Some("link") {
            let link = install_root.join(safe_relative_path(relative_name)?);
            ensure_runtime_link_target(&canonical_install_root, &link)?;
        }
    }

    let version_name = entry.version.get("name").ok_or_else(|| {
        format!("El manifiesto Java no incluye el nombre de versión de {component}")
    })?;
    write_runtime_metadata(&runtime_root.join(".version"), version_name.as_bytes())?;
    let mut sha1_index = String::new();
    for relative in installed_files {
        let path = install_root.join(&relative);
        let hash = sha1_file(&path)?;
        let elapsed = fs::metadata(&path)
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .map_or(0, |duration| duration.as_nanos());
        sha1_index.push_str(&format!("{} /#// {} {}\n", relative.display(), hash, elapsed));
    }
    write_runtime_metadata(
        &runtime_root.join(format!("{component}.sha1")),
        sha1_index.as_bytes(),
    )?;
    Ok(())
}

fn runtime_platform() -> Result<String, String> {
    match (env::consts::OS, env::consts::ARCH) {
        ("windows", "x86") => Ok("windows-x86".into()),
        ("windows", "aarch64") => Ok("windows-arm64".into()),
        ("windows", _) => Ok("windows-x64".into()),
        ("linux", "x86") => Ok("linux-i386".into()),
        ("linux", _) => Ok("linux".into()),
        ("macos", "aarch64") => Ok("mac-os-arm64".into()),
        ("macos", _) => Ok("mac-os".into()),
        (os, _) => Err(format!("Mojang no ofrece runtime automático para {os}")),
    }
}

fn safe_relative_path(value: &str) -> Result<PathBuf, String> {
    let path = Path::new(value);
    if value.is_empty()
        || !path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        return Err(format!("Ruta insegura en el manifiesto Java: {value}"));
    }
    Ok(path.to_path_buf())
}

fn validate_link_target(link: &Path, target: &str) -> Result<(), String> {
    let target_path = Path::new(target);
    if target.is_empty()
        || target_path
            .components()
            .any(|component| matches!(component, Component::RootDir | Component::Prefix(_)))
    {
        return Err(format!("Destino de enlace Java inseguro: {target}"));
    }
    let mut resolved = link.parent().unwrap_or_else(|| Path::new("")).to_path_buf();
    for component in target_path.components() {
        match component {
            Component::Normal(part) => resolved.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                if !resolved.pop() {
                    return Err(format!("El enlace Java sale del runtime: {target}"));
                }
            }
            Component::RootDir | Component::Prefix(_) => unreachable!(),
        }
    }
    Ok(())
}

fn ensure_normal_directory(path: &Path) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(()),
        Ok(_) => Err(format!(
            "No se puede instalar Java porque la ruta no es un directorio normal: {}",
            path.display()
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match fs::create_dir(path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    match fs::symlink_metadata(path) {
                        Ok(metadata) if metadata.file_type().is_dir() => Ok(()),
                        Ok(_) => Err(format!(
                            "No se puede instalar Java porque la ruta no es un directorio normal: {}",
                            path.display()
                        )),
                        Err(error) => Err(format!(
                            "No se pudo revisar el directorio del runtime {}: {error}",
                            path.display()
                        )),
                    }
                }
                Err(error) => Err(format!(
                    "No se pudo crear el directorio del runtime {}: {error}",
                    path.display()
                )),
            }
        }
        Err(error) => Err(format!(
            "No se pudo revisar el directorio del runtime {}: {error}",
            path.display()
        )),
    }
}

fn ensure_runtime_directory_tree(root: &Path, path: &Path) -> Result<(), String> {
    let relative = path
        .strip_prefix(root)
        .map_err(|_| format!("La ruta Java sale del runtime: {}", path.display()))?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return Err(format!(
                "Ruta de directorio Java no válida: {}",
                path.display()
            ));
        };
        current.push(part);
        ensure_normal_directory(&current)?;
    }
    Ok(())
}

fn ensure_runtime_link_target(root: &Path, link: &Path) -> Result<(), String> {
    let canonical = fs::canonicalize(link)
        .map_err(|error| {
            format!(
                "No se pudo validar el destino del enlace Java {}: {error}",
                link.display()
            )
        })?;
    if canonical.starts_with(root) {
        Ok(())
    } else {
        Err(format!(
            "El enlace Java sale del runtime: {}",
            link.display()
        ))
    }
}

fn write_runtime_metadata(path: &Path, contents: &[u8]) -> Result<(), String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => {}
        Ok(_) => {
            return Err(format!(
                "Los metadatos existentes del runtime no son un archivo normal: {}",
                path.display()
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "No se pudieron revisar los metadatos del runtime {}: {error}",
                path.display()
            ));
        }
    }
    let mut output = AtomicWriteFile::options()
        .open(path)
        .map_err(|error| format!("No se pudieron preparar los metadatos Java: {error}"))?;
    output
        .write_all(contents)
        .map_err(|error| format!("No se pudieron escribir los metadatos Java: {error}"))?;
    output
        .commit()
        .map_err(|error| format!("No se pudieron confirmar los metadatos Java: {error}"))
}

fn install_runtime_file(
    client: &reqwest::blocking::Client,
    destination: &Path,
    raw: &RuntimeDownload,
    compressed: Option<&RuntimeDownload>,
    runtime_root: &Path,
) -> Result<(), String> {
    match fs::symlink_metadata(destination) {
        Ok(metadata) if metadata.file_type().is_file() => {
            if sha1_file(destination)? == raw.sha1 {
                return Ok(());
            }
        }
        Ok(_) => {
            return Err(format!(
                "La ruta Java existente no es un archivo normal: {}",
                destination.display()
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(format!(
                "No se pudo inspeccionar {}: {error}",
                destination.display()
            ));
        }
    }
    let parent = destination
        .parent()
        .ok_or_else(|| "El archivo Java no tiene directorio padre".to_string())?;
    ensure_runtime_directory_tree(runtime_root, parent)?;
    let canonical_parent = fs::canonicalize(parent)
        .map_err(|error| format!("No se pudo resolver la carpeta de archivos Java: {error}"))?;
    if !canonical_parent.starts_with(runtime_root) {
        return Err(format!(
            "La ruta Java sale del directorio del runtime: {}",
            destination.display()
        ));
    }
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temp_path = destination.with_file_name(format!(
        ".{}.download-{}",
        destination.file_name().unwrap_or_default().to_string_lossy(),
        nonce
    ));
    let result = (|| {
        let mut response = client
            .get(compressed.unwrap_or(raw).url.as_str())
            .send()
            .and_then(reqwest::blocking::Response::error_for_status)
            .map_err(|error| format!("No se pudo descargar {}: {error}", destination.display()))?;
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .map_err(|error| format!("No se pudo guardar {}: {error}", destination.display()))?;
        if compressed.is_some() {
            decode_lzma(response, &mut output).map_err(|error| {
                format!("No se pudo descomprimir {}: {error}", destination.display())
            })?;
        } else {
            io::copy(&mut response, &mut output)
                .map_err(|error| format!("No se pudo escribir {}: {error}", destination.display()))?;
        }
        output.flush().map_err(|error| {
            format!("No se pudo terminar de guardar {}: {error}", destination.display())
        })?;
        drop(output);
        if sha1_file(&temp_path)? != raw.sha1 {
            return Err(format!(
                "El SHA-1 del archivo Java {} no coincide con Mojang",
                destination.display()
            ));
        }
        match fs::symlink_metadata(destination) {
            Ok(metadata) if metadata.file_type().is_file() => {
                fs::remove_file(destination).map_err(|error| {
                    format!("No se pudo sustituir {}: {error}", destination.display())
                })?;
            }
            Ok(_) => {
                return Err(format!(
                    "La ruta Java cambió y ya no es un archivo normal: {}",
                    destination.display()
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "No se pudo inspeccionar {} antes de sustituirlo: {error}",
                    destination.display()
                ));
            }
        }
        fs::rename(&temp_path, destination)
            .map_err(|error| format!("No se pudo instalar {}: {error}", destination.display()))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result
}

fn decode_lzma(input: impl Read, output: impl Write) -> Result<(), String> {
    let stream = Stream::new_lzma_decoder(LZMA_MEMORY_LIMIT)
        .map_err(|error| format!("No se pudo inicializar el decodificador LZMA: {error}"))?;
    let mut decoder = XzDecoder::new_stream(input, stream);
    let mut output = output;
    io::copy(&mut decoder, &mut output)
        .map_err(|error| format!("No se pudo descomprimir el archivo LZMA: {error}"))?;
    Ok(())
}

fn sha1_file(path: &Path) -> Result<String, String> {
    let mut file = fs::File::open(path)
        .map_err(|error| format!("No se pudo abrir {} para verificarlo: {error}", path.display()))?;
    let mut hasher = Sha1::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("No se pudo verificar {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn set_executable(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = fs::metadata(path)
            .map_err(|error| format!("No se pudieron leer permisos de {}: {error}", path.display()))?
            .permissions();
        permissions.set_mode(permissions.mode() | 0o111);
        fs::set_permissions(path, permissions)
            .map_err(|error| format!("No se pudieron habilitar permisos de {}: {error}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{decode_lzma, safe_relative_path, validate_link_target};
    #[cfg(unix)]
    use super::{
        ensure_normal_directory, ensure_runtime_link_target, install_runtime_file, sha1_file,
        write_runtime_metadata, RuntimeDownload,
    };
    use std::{io::Cursor, path::Path};
    #[cfg(unix)]
    use std::{fs, time::SystemTime};

    #[cfg(unix)]
    fn temporary_directory(name: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "eternalcraft-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn runtime_paths_reject_absolute_and_parent_traversal() {
        assert!(safe_relative_path("bin/java").is_ok());
        assert!(safe_relative_path("../outside").is_err());
        assert!(safe_relative_path("/tmp/outside").is_err());
    }

    #[test]
    fn mojang_lzma_alone_runtime_files_decode_correctly() {
        let compressed = [
            93, 0, 0, 128, 0, 255, 255, 255, 255, 255, 255, 255, 255, 0, 38, 155, 201, 70,
            36, 25, 35, 215, 1, 87, 21, 105, 204, 240, 218, 37, 175, 236, 157, 30, 63, 17,
            6, 201, 45, 239, 247, 159, 105, 16, 123, 82, 90, 216, 104, 226, 245, 207, 205,
            162, 125, 61, 196, 67, 255, 248, 20, 192, 0,
        ];
        let mut decoded = Vec::new();
        decode_lzma(Cursor::new(compressed), &mut decoded).unwrap();
        assert_eq!(decoded, b"Mojang runtime uses LZMA-alone, not XZ.");
    }

    #[test]
    fn runtime_links_can_use_parent_segments_without_leaving_runtime() {
        assert!(validate_link_target(
            Path::new("legal/java.compiler/LICENSE"),
            "../java.base/LICENSE"
        )
        .is_ok());
        assert!(validate_link_target(Path::new("bin/java"), "../../outside").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn runtime_files_cannot_follow_existing_symlinks() {
        let root = temporary_directory("runtime-file-symlink");
        let external = root.join("outside.bin");
        let runtime = root.join("instance/runtime/java");
        fs::create_dir_all(&runtime).unwrap();
        fs::write(&external, b"outside runtime").unwrap();
        let destination = runtime.join("java");
        std::os::unix::fs::symlink(&external, &destination).unwrap();
        let expected_hash = sha1_file(&external).unwrap();
        let client = reqwest::blocking::Client::new();
        let download = RuntimeDownload {
            sha1: expected_hash,
            url: "https://example.invalid/runtime".into(),
        };

        assert!(install_runtime_file(
            &client,
            &destination,
            &download,
            None,
            &fs::canonicalize(&runtime).unwrap()
        )
        .is_err());
        assert_eq!(fs::read(&external).unwrap(), b"outside runtime");
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn runtime_file_parent_symlink_cannot_redirect_outside_component() {
        let root = temporary_directory("runtime-parent-symlink");
        let game = root.join("instance");
        let runtime = game.join("runtime/java");
        let other_game_data = game.join("mods");
        fs::create_dir_all(&runtime).unwrap();
        fs::create_dir_all(&other_game_data).unwrap();
        std::os::unix::fs::symlink(&other_game_data, runtime.join("redirect")).unwrap();

        assert!(super::ensure_runtime_directory_tree(
            &fs::canonicalize(&runtime).unwrap(),
            &runtime.join("redirect")
        )
        .is_err());
        assert!(!other_game_data.join("created").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn runtime_component_directories_reject_symlinks() {
        let root = temporary_directory("runtime-directory-symlink");
        let outside = root.join("outside");
        fs::create_dir_all(&outside).unwrap();
        let runtime = root.join("runtime");
        std::os::unix::fs::symlink(&outside, &runtime).unwrap();

        assert!(ensure_normal_directory(&runtime).is_err());
        assert!(fs::read_dir(&outside).unwrap().next().is_none());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn runtime_link_cannot_escape_through_an_existing_target_symlink() {
        let root = temporary_directory("runtime-link-target-symlink");
        let runtime = root.join("runtime");
        let outside = root.join("outside");
        let bin = runtime.join("bin");
        let lib = runtime.join("lib");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&lib).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("java"), b"outside runtime target").unwrap();
        std::os::unix::fs::symlink(outside.join("java"), lib.join("java")).unwrap();
        let link = bin.join("java");
        std::os::unix::fs::symlink("../lib/java", &link).unwrap();

        assert!(ensure_runtime_link_target(&fs::canonicalize(&runtime).unwrap(), &link).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn runtime_metadata_rejects_existing_symlinks() {
        let root = temporary_directory("runtime-metadata-symlink");
        let external = root.join("outside.txt");
        let metadata = root.join(".version");
        fs::write(&external, b"keep this file").unwrap();
        std::os::unix::fs::symlink(&external, &metadata).unwrap();

        assert!(write_runtime_metadata(&metadata, b"17.0.1").is_err());
        assert_eq!(fs::read(&external).unwrap(), b"keep this file");
        fs::remove_dir_all(root).unwrap();
    }
}
