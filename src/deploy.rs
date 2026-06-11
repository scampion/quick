use std::collections::HashSet;
use std::fs;
use std::io::{self, Cursor, Read};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::hosting::valid_site_name;

pub const MANIFEST_PATH: &str = ".quick-manifest.json";

#[derive(Deserialize, Serialize)]
pub struct SiteManifest {
    pub files: Vec<String>,
}

pub fn deploy(source: &Path, sites_dir: &Path, site: &str) -> io::Result<()> {
    if !valid_site_name(site) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "site names must contain only lowercase letters, digits, and hyphens",
        ));
    }
    if !source.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "source must be a directory",
        ));
    }
    deploy_files(read_directory(source)?, sites_dir, site)
}

pub fn read_directory(source: &Path) -> io::Result<Vec<(PathBuf, Vec<u8>)>> {
    if !source.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            "source must be a directory",
        ));
    }

    let mut files = Vec::new();
    collect_directory(source, source, &mut files)?;
    validate_files(&files)?;
    Ok(files)
}

pub fn deploy_files(
    files: impl IntoIterator<Item = (PathBuf, Vec<u8>)>,
    sites_dir: &Path,
    site: &str,
) -> io::Result<()> {
    if !valid_site_name(site) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "site names must contain only lowercase letters, digits, and hyphens",
        ));
    }

    let files = prepare_files(files.into_iter().collect())?;
    let staging = create_staging_dir(sites_dir, site)?;
    let result = (|| {
        for (relative_path, content) in files {
            let relative_path = safe_upload_path(&relative_path)?;

            let destination = staging.join(relative_path);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(destination, content)?;
        }

        promote(&staging, sites_dir, site)
    })();

    if let Err(error) = result {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }

    Ok(())
}

#[cfg(test)]
pub fn deploy_zip(
    archive_bytes: Vec<u8>,
    sites_dir: &Path,
    site: &str,
    max_files: usize,
    max_uncompressed_bytes: usize,
) -> io::Result<()> {
    let files = extract_zip_files(archive_bytes, max_files, max_uncompressed_bytes)?;
    deploy_files(files, sites_dir, site)
}

pub fn extract_zip_files(
    archive_bytes: Vec<u8>,
    max_files: usize,
    max_uncompressed_bytes: usize,
) -> io::Result<Vec<(PathBuf, Vec<u8>)>> {
    let mut archive = zip::ZipArchive::new(Cursor::new(archive_bytes)).map_err(zip_error)?;
    if archive.len() > max_files {
        return Err(invalid_input(format!(
            "ZIP archive exceeds the limit of {max_files} entries"
        )));
    }

    let mut paths = Vec::new();
    let mut total_size = 0_u64;
    for index in 0..archive.len() {
        let file = archive.by_index(index).map_err(zip_error)?;
        if file.encrypted() {
            return Err(invalid_input("encrypted ZIP files are not supported"));
        }
        if file.is_symlink() {
            return Err(invalid_input(
                "symbolic links in ZIP files are not supported",
            ));
        }
        let path = file
            .enclosed_name()
            .ok_or_else(|| invalid_input(format!("unsafe ZIP path: {}", file.name())))?;
        if path.as_os_str().is_empty() || is_ignored_archive_path(&path) {
            continue;
        }

        if file.is_file() {
            total_size = total_size
                .checked_add(file.size())
                .ok_or_else(|| invalid_input("ZIP archive is too large"))?;
            if total_size > max_uncompressed_bytes as u64 {
                return Err(invalid_input(format!(
                    "ZIP contents exceed the limit of {} MiB",
                    max_uncompressed_bytes / (1024 * 1024)
                )));
            }
            paths.push(path);
        }
    }

    let root = common_archive_root(&paths);
    let mut files = Vec::with_capacity(paths.len());
    let mut seen = HashSet::with_capacity(paths.len());
    let mut actual_size = 0_usize;
    for index in 0..archive.len() {
        let mut file = archive.by_index(index).map_err(zip_error)?;
        if !file.is_file() {
            continue;
        }

        let original_path = file
            .enclosed_name()
            .ok_or_else(|| invalid_input(format!("unsafe ZIP path: {}", file.name())))?;
        if is_ignored_archive_path(&original_path) {
            continue;
        }
        let relative_path = match &root {
            Some(root) => original_path
                .strip_prefix(root)
                .map(Path::to_path_buf)
                .map_err(|_| invalid_input("ZIP archive has an inconsistent root directory"))?,
            None => original_path,
        };
        let relative_path = safe_upload_path(&relative_path)?.to_path_buf();
        if !seen.insert(relative_path.clone()) {
            return Err(invalid_input(format!(
                "duplicate ZIP path: {}",
                relative_path.display()
            )));
        }

        let remaining = max_uncompressed_bytes.saturating_sub(actual_size);
        let mut content = Vec::with_capacity(file.size().min(remaining as u64) as usize);
        file.by_ref()
            .take(remaining as u64 + 1)
            .read_to_end(&mut content)?;
        actual_size += content.len();
        if actual_size > max_uncompressed_bytes {
            return Err(invalid_input(format!(
                "ZIP contents exceed the limit of {} MiB",
                max_uncompressed_bytes / (1024 * 1024)
            )));
        }
        files.push((relative_path, content));
    }

    if files.is_empty() {
        return Err(invalid_input("ZIP archive contains no files"));
    }
    validate_files(&files)?;
    Ok(files)
}

pub fn validate_files(files: &[(PathBuf, Vec<u8>)]) -> io::Result<()> {
    if files.is_empty() {
        return Err(invalid_input("upload must contain at least one file"));
    }

    let mut seen = HashSet::with_capacity(files.len());
    for (path, _) in files {
        let path = safe_upload_path(path)?;
        if path == Path::new(MANIFEST_PATH) {
            return Err(invalid_input(format!(
                "{MANIFEST_PATH} is reserved by Quick"
            )));
        }
        if !seen.insert(path.to_path_buf()) {
            return Err(invalid_input(format!(
                "duplicate upload path: {}",
                path.display()
            )));
        }
    }
    Ok(())
}

pub fn prepare_files(mut files: Vec<(PathBuf, Vec<u8>)>) -> io::Result<Vec<(PathBuf, Vec<u8>)>> {
    validate_files(&files)?;
    let mut paths: Vec<_> = files
        .iter()
        .map(|(path, _)| path.to_string_lossy().replace('\\', "/"))
        .collect();
    paths.sort();
    let manifest = serde_json::to_vec(&SiteManifest { files: paths })
        .map_err(|error| io::Error::other(format!("could not encode site manifest: {error}")))?;
    files.push((PathBuf::from(MANIFEST_PATH), manifest));
    Ok(files)
}

fn common_archive_root(paths: &[PathBuf]) -> Option<PathBuf> {
    let first_root = paths.first()?.components().next()?;
    if paths
        .iter()
        .all(|path| path.components().count() >= 2 && path.components().next() == Some(first_root))
    {
        Some(PathBuf::from(first_root.as_os_str()))
    } else {
        None
    }
}

fn is_ignored_archive_path(path: &Path) -> bool {
    path.components()
        .next()
        .is_some_and(|component| component.as_os_str() == "__MACOSX")
        || path
            .file_name()
            .is_some_and(|file_name| file_name == ".DS_Store")
}

fn invalid_input(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn zip_error(error: zip::result::ZipError) -> io::Error {
    invalid_input(format!("invalid ZIP archive: {error}"))
}

fn create_staging_dir(sites_dir: &Path, site: &str) -> io::Result<PathBuf> {
    fs::create_dir_all(sites_dir)?;
    let staging = sites_dir.join(format!(".{site}-{}.tmp", nonce()));
    fs::create_dir(&staging)?;
    Ok(staging)
}

fn promote(staging: &Path, sites_dir: &Path, site: &str) -> io::Result<()> {
    let nonce = nonce();
    let destination = sites_dir.join(site);
    let previous = sites_dir.join(format!(".{site}-{nonce}.old"));

    if destination.exists() {
        fs::rename(&destination, &previous)?;
    }

    if let Err(error) = fs::rename(staging, &destination) {
        if previous.exists() {
            let _ = fs::rename(&previous, &destination);
        }
        return Err(error);
    }

    if previous.exists() {
        fs::remove_dir_all(previous)?;
    }

    Ok(())
}

fn nonce() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

fn safe_upload_path(path: &Path) -> io::Result<&Path> {
    let valid = !path.as_os_str().is_empty()
        && !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)));

    valid.then_some(path).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid upload path: {}", path.display()),
        )
    })
}

fn collect_directory(
    root: &Path,
    directory: &Path,
    files: &mut Vec<(PathBuf, Vec<u8>)>,
) -> io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "symbolic links are not supported: {}",
                    entry.path().display()
                ),
            ));
        } else if file_type.is_dir() {
            collect_directory(root, &entry.path(), files)?;
        } else if file_type.is_file() {
            let relative_path = entry
                .path()
                .strip_prefix(root)
                .map(Path::to_path_buf)
                .map_err(|_| invalid_input("source file is outside the deployment root"))?;
            files.push((relative_path, fs::read(entry.path())?));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::write::SimpleFileOptions;

    fn make_zip(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (path, content) in files {
            writer
                .start_file(*path, SimpleFileOptions::default())
                .unwrap();
            writer.write_all(content).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn deploys_and_replaces_a_site() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        let sites = temp.path().join("sites");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("index.html"), "first").unwrap();

        deploy(&source, &sites, "demo").unwrap();
        assert_eq!(
            fs::read_to_string(sites.join("demo/index.html")).unwrap(),
            "first"
        );

        fs::write(source.join("index.html"), "second").unwrap();
        deploy(&source, &sites, "demo").unwrap();
        assert_eq!(
            fs::read_to_string(sites.join("demo/index.html")).unwrap(),
            "second"
        );
    }

    #[test]
    fn deploys_a_directory_without_an_index_file() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("notes.txt"), "hello").unwrap();

        deploy(&source, &temp.path().join("sites"), "demo").unwrap();
        assert_eq!(
            fs::read_to_string(temp.path().join("sites/demo/notes.txt")).unwrap(),
            "hello"
        );
        let manifest: SiteManifest = serde_json::from_slice(
            &fs::read(temp.path().join("sites/demo/.quick-manifest.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(manifest.files, ["notes.txt"]);
    }

    #[test]
    fn deploys_uploaded_files_and_rejects_traversal() {
        let temp = tempfile::tempdir().unwrap();
        let sites = temp.path().join("sites");

        deploy_files(
            [
                (PathBuf::from("index.html"), b"hello".to_vec()),
                (PathBuf::from("assets/app.js"), b"app".to_vec()),
            ],
            &sites,
            "upload",
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(sites.join("upload/assets/app.js")).unwrap(),
            "app"
        );

        let error = deploy_files(
            [
                (PathBuf::from("index.html"), b"hello".to_vec()),
                (PathBuf::from("../secret"), b"nope".to_vec()),
            ],
            &sites,
            "unsafe",
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(!sites.join("unsafe").exists());
    }

    #[test]
    fn deploys_zip_and_strips_a_single_root_directory() {
        let temp = tempfile::tempdir().unwrap();
        let sites = temp.path().join("sites");
        let archive = make_zip(&[
            ("my-site/index.html", b"zip home"),
            ("my-site/assets/app.js", b"zip app"),
        ]);

        deploy_zip(archive, &sites, "zip-site", 100, 1024 * 1024).unwrap();

        assert_eq!(
            fs::read_to_string(sites.join("zip-site/index.html")).unwrap(),
            "zip home"
        );
        assert_eq!(
            fs::read_to_string(sites.join("zip-site/assets/app.js")).unwrap(),
            "zip app"
        );
    }

    #[test]
    fn accepts_zip_archives_without_an_index_file() {
        let temp = tempfile::tempdir().unwrap();
        let sites = temp.path().join("sites");
        let archive = make_zip(&[
            ("my-files/readme.md", b"hello"),
            ("my-files/App.jsx", b"export default () => <h1>Hello</h1>"),
        ]);

        deploy_zip(archive, &sites, "files", 100, 1024 * 1024).unwrap();

        assert!(sites.join("files/readme.md").is_file());
        assert!(sites.join("files/App.jsx").is_file());
    }

    #[test]
    fn ignores_macos_metadata_in_zip_archives() {
        let temp = tempfile::tempdir().unwrap();
        let sites = temp.path().join("sites");
        let archive = make_zip(&[
            ("my-site/index.html", b"zip home"),
            ("my-site/.DS_Store", b"metadata"),
            ("__MACOSX/my-site/._index.html", b"metadata"),
        ]);

        deploy_zip(archive, &sites, "mac-zip", 100, 1024 * 1024).unwrap();

        assert_eq!(
            fs::read_to_string(sites.join("mac-zip/index.html")).unwrap(),
            "zip home"
        );
        assert!(!sites.join("mac-zip/.DS_Store").exists());
        assert!(!sites.join("mac-zip/__MACOSX").exists());
    }

    #[test]
    fn rejects_unsafe_or_oversized_zip_archives() {
        let temp = tempfile::tempdir().unwrap();
        let sites = temp.path().join("sites");
        let unsafe_archive = make_zip(&[("index.html", b"home"), ("../outside.txt", b"outside")]);
        let error = deploy_zip(unsafe_archive, &sites, "unsafe-zip", 100, 1024 * 1024).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(!sites.join("unsafe-zip").exists());

        let large_archive = make_zip(&[("index.html", &[b'x'; 128])]);
        let error = deploy_zip(large_archive, &sites, "large-zip", 100, 64).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(!sites.join("large-zip").exists());

        let too_many_files = make_zip(&[("index.html", b"home"), ("asset.txt", b"asset")]);
        let error = deploy_zip(too_many_files, &sites, "many-zip", 1, 1024).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(!sites.join("many-zip").exists());
    }

    #[test]
    fn rejects_symbolic_links_in_zip_archives() {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file("index.html", SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"home").unwrap();
        writer
            .add_symlink("linked-file", "/etc/passwd", SimpleFileOptions::default())
            .unwrap();
        let archive = writer.finish().unwrap().into_inner();
        let temp = tempfile::tempdir().unwrap();

        let error =
            deploy_zip(archive, &temp.path().join("sites"), "links", 100, 1024).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(!temp.path().join("sites/links").exists());
    }
}
