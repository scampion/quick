use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::hosting::valid_site_name;

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
    if !source.join("index.html").is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "source must contain an index.html file",
        ));
    }

    let staging = create_staging_dir(sites_dir, site)?;

    if let Err(error) =
        copy_directory(source, &staging).and_then(|_| promote(&staging, sites_dir, site))
    {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }

    Ok(())
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

    let staging = create_staging_dir(sites_dir, site)?;
    let result = (|| {
        let mut has_index = false;
        for (relative_path, content) in files {
            let relative_path = safe_upload_path(&relative_path)?;
            has_index |= relative_path == Path::new("index.html");

            let destination = staging.join(relative_path);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(destination, content)?;
        }

        if !has_index {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "upload must contain an index.html file at its root",
            ));
        }

        promote(&staging, sites_dir, site)
    })();

    if let Err(error) = result {
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }

    Ok(())
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

fn copy_directory(source: &Path, destination: &Path) -> io::Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let target = destination.join(entry.file_name());

        if file_type.is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "symbolic links are not supported: {}",
                    entry.path().display()
                ),
            ));
        } else if file_type.is_dir() {
            copy_directory(&entry.path(), &target)?;
        } else if file_type.is_file() {
            fs::copy(entry.path(), target)?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn requires_an_index_file() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();

        let error = deploy(&source, &temp.path().join("sites"), "demo").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
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
}
