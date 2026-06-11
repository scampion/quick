use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use s3::Bucket;
use s3::Region;
use s3::creds::Credentials;
use serde::{Deserialize, Serialize};

use crate::deploy;

pub struct StoredObject {
    pub body: Vec<u8>,
    pub content_type: String,
}

#[async_trait]
pub trait Storage: Send + Sync {
    async fn get(&self, site: &str, path: &Path) -> io::Result<Option<StoredObject>>;
    async fn deploy(&self, site: &str, files: Vec<(PathBuf, Vec<u8>)>) -> io::Result<()>;
    fn description(&self) -> String;
}

pub type SharedStorage = Arc<dyn Storage>;

pub struct LocalStorage {
    sites_dir: PathBuf,
}

impl LocalStorage {
    pub fn new(sites_dir: PathBuf) -> Self {
        Self { sites_dir }
    }
}

#[async_trait]
impl Storage for LocalStorage {
    async fn get(&self, site: &str, path: &Path) -> io::Result<Option<StoredObject>> {
        let full_path = self.sites_dir.join(site).join(path);
        match fs::read(&full_path) {
            Ok(body) => Ok(Some(StoredObject {
                body,
                content_type: content_type(path),
            })),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    async fn deploy(&self, site: &str, files: Vec<(PathBuf, Vec<u8>)>) -> io::Result<()> {
        deploy::deploy_files(files, &self.sites_dir, site)
    }

    fn description(&self) -> String {
        format!("local:{}", self.sites_dir.display())
    }
}

#[derive(Clone)]
pub struct S3Storage {
    bucket: Box<Bucket>,
    prefix: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct CurrentRelease {
    release: String,
}

impl S3Storage {
    pub fn new(
        bucket_name: &str,
        region_name: &str,
        endpoint: Option<&str>,
        prefix: &str,
        path_style: bool,
    ) -> io::Result<Self> {
        let region = match endpoint {
            Some(endpoint) => Region::Custom {
                region: region_name.to_owned(),
                endpoint: endpoint.trim_end_matches('/').to_owned(),
            },
            None => region_name
                .parse()
                .map_err(|error| invalid_data(format!("invalid S3 region: {error}")))?,
        };
        let credentials = Credentials::default()
            .map_err(|error| invalid_data(format!("could not load S3 credentials: {error}")))?;
        Self::with_credentials(bucket_name, region, credentials, prefix, path_style)
    }

    fn with_credentials(
        bucket_name: &str,
        region: Region,
        credentials: Credentials,
        prefix: &str,
        path_style: bool,
    ) -> io::Result<Self> {
        let bucket = Bucket::new(bucket_name, region, credentials)
            .map_err(|error| invalid_data(format!("could not configure S3 bucket: {error}")))?;
        let bucket = if path_style {
            bucket.with_path_style()
        } else {
            bucket
        };

        Ok(Self {
            bucket,
            prefix: prefix.trim_matches('/').to_owned(),
        })
    }

    fn key(&self, suffix: &str) -> String {
        if self.prefix.is_empty() {
            suffix.to_owned()
        } else {
            format!("{}/{}", self.prefix, suffix)
        }
    }

    fn current_key(&self, site: &str) -> String {
        self.key(&format!("sites/{site}/current.json"))
    }

    fn object_key(&self, site: &str, release: &str, path: &Path) -> String {
        self.key(&format!(
            "sites/{site}/releases/{release}/{}",
            path.to_string_lossy()
        ))
    }

    async fn current_release(&self, site: &str) -> io::Result<Option<String>> {
        let response = self
            .bucket
            .get_object(self.current_key(site))
            .await
            .map_err(s3_error)?;
        match response.status_code() {
            200 => {
                let current: CurrentRelease =
                    serde_json::from_slice(response.as_slice()).map_err(|error| {
                        invalid_data(format!("invalid S3 current release pointer: {error}"))
                    })?;
                Ok(Some(current.release))
            }
            404 => Ok(None),
            status => Err(s3_status("read current release pointer", status)),
        }
    }
}

#[async_trait]
impl Storage for S3Storage {
    async fn get(&self, site: &str, path: &Path) -> io::Result<Option<StoredObject>> {
        let Some(release) = self.current_release(site).await? else {
            return Ok(None);
        };
        let response = self
            .bucket
            .get_object(self.object_key(site, &release, path))
            .await
            .map_err(s3_error)?;

        match response.status_code() {
            200 => Ok(Some(StoredObject {
                body: response.to_vec(),
                content_type: content_type(path),
            })),
            404 => Ok(None),
            status => Err(s3_status("read site object", status)),
        }
    }

    async fn deploy(&self, site: &str, files: Vec<(PathBuf, Vec<u8>)>) -> io::Result<()> {
        let files = deploy::prepare_files(files)?;
        let release = release_id();

        for (path, body) in files {
            let response = self
                .bucket
                .put_object_with_content_type(
                    self.object_key(site, &release, &path),
                    &body,
                    &content_type(&path),
                )
                .await
                .map_err(s3_error)?;
            if !(200..300).contains(&response.status_code()) {
                return Err(s3_status("upload site object", response.status_code()));
            }
        }

        let pointer = serde_json::to_vec(&CurrentRelease { release }).map_err(|error| {
            invalid_data(format!("could not encode S3 release pointer: {error}"))
        })?;
        let response = self
            .bucket
            .put_object_with_content_type(
                self.current_key(site),
                &pointer,
                "application/json; charset=utf-8",
            )
            .await
            .map_err(s3_error)?;
        if !(200..300).contains(&response.status_code()) {
            return Err(s3_status(
                "publish current release pointer",
                response.status_code(),
            ));
        }

        Ok(())
    }

    fn description(&self) -> String {
        format!(
            "s3://{}/{}",
            self.bucket.name(),
            self.prefix.trim_end_matches('/')
        )
        .trim_end_matches('/')
        .to_owned()
    }
}

pub fn block_on<T>(future: impl std::future::Future<Output = T>) -> io::Result<T> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| io::Error::other(format!("could not start async runtime: {error}")))?;
    Ok(runtime.block_on(future))
}

fn release_id() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .to_string()
}

fn content_type(path: &Path) -> String {
    mime_guess::from_path(path)
        .first_or_octet_stream()
        .essence_str()
        .to_owned()
}

fn s3_error(error: s3::error::S3Error) -> io::Error {
    io::Error::other(format!("S3 request failed: {error}"))
}

fn s3_status(action: &str, status: u16) -> io::Error {
    io::Error::other(format!("S3 could not {action}: HTTP {status}"))
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    #[test]
    fn content_types_follow_object_paths() {
        assert_eq!(content_type(Path::new("index.html")), "text/html");
        assert_eq!(content_type(Path::new("assets/app.js")), "text/javascript");
    }

    #[test]
    fn builds_stable_s3_keys() {
        let credentials = Credentials::new(Some("test"), Some("test"), None, None, None).unwrap();
        let storage = S3Storage::with_credentials(
            "bucket",
            Region::Custom {
                region: "us-east-1".to_owned(),
                endpoint: "http://localhost:9000".to_owned(),
            },
            credentials,
            "/quick/",
            true,
        )
        .unwrap();

        assert_eq!(storage.current_key("demo"), "quick/sites/demo/current.json");
        assert_eq!(
            storage.object_key("demo", "123", Path::new("assets/app.js")),
            "quick/sites/demo/releases/123/assets/app.js"
        );
    }

    #[test]
    fn deploys_and_reads_through_an_s3_compatible_endpoint() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let mut objects = HashMap::new();
            for _ in 0..6 {
                let (mut stream, _) = listener.accept().unwrap();
                handle_s3_request(&mut stream, &mut objects);
            }
            objects
        });

        let credentials = Credentials::new(Some("test"), Some("test"), None, None, None).unwrap();
        let storage = S3Storage::with_credentials(
            "bucket",
            Region::Custom {
                region: "us-east-1".to_owned(),
                endpoint: format!("http://{address}"),
            },
            credentials,
            "quick",
            true,
        )
        .unwrap();

        block_on(storage.deploy(
            "demo",
            vec![
                (PathBuf::from("index.html"), b"home".to_vec()),
                (PathBuf::from("assets/app.js"), b"app".to_vec()),
            ],
        ))
        .unwrap()
        .unwrap();
        let object = block_on(storage.get("demo", Path::new("assets/app.js")))
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(object.body, b"app");
        assert_eq!(object.content_type, "text/javascript");

        let objects = server.join().unwrap();
        assert!(objects.contains_key("/bucket/quick/sites/demo/current.json"));
        assert!(objects.keys().any(|key| key.ends_with("/assets/app.js")));
        assert!(
            objects
                .keys()
                .any(|key| key.ends_with("/.quick-manifest.json"))
        );
    }

    fn handle_s3_request(stream: &mut TcpStream, objects: &mut HashMap<String, Vec<u8>>) {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 4096];
        let header_end;
        loop {
            let count = stream.read(&mut buffer).unwrap();
            request.extend_from_slice(&buffer[..count]);
            if let Some(position) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                header_end = position + 4;
                break;
            }
        }

        let headers = String::from_utf8_lossy(&request[..header_end]).into_owned();
        let mut lines = headers.lines();
        let request_line = lines.next().unwrap();
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap().to_owned();
        let path = parts.next().unwrap().split('?').next().unwrap().to_owned();
        let content_length = lines
            .find_map(|line| {
                line.split_once(':')
                    .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                    .and_then(|(_, value)| value.trim().parse::<usize>().ok())
            })
            .unwrap_or(0);

        while request.len() < header_end + content_length {
            let count = stream.read(&mut buffer).unwrap();
            request.extend_from_slice(&buffer[..count]);
        }
        let body = request[header_end..header_end + content_length].to_vec();

        match method.as_str() {
            "PUT" => {
                objects.insert(path, body);
                write_http_response(stream, 200, &[]);
            }
            "GET" => match objects.get(&path) {
                Some(body) => write_http_response(stream, 200, body),
                None => write_http_response(stream, 404, &[]),
            },
            _ => write_http_response(stream, 405, &[]),
        }
    }

    fn write_http_response(stream: &mut TcpStream, status: u16, body: &[u8]) {
        let reason = if status == 200 { "OK" } else { "Not Found" };
        write!(
            stream,
            "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        stream.write_all(body).unwrap();
        stream.flush().unwrap();
    }
}
