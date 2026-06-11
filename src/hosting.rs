use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use base64::Engine;
use http::{Method, Response, StatusCode};
use pingora::apps::http_app::ServeHttp;
use pingora::protocols::http::ServerSession;
use serde::{Deserialize, Serialize};

use crate::deploy;
use crate::storage::SharedStorage;

const DASHBOARD: &str = include_str!("dashboard.html");
const MAX_REQUEST_BYTES: usize = 35 * 1024 * 1024;
const MAX_DEPLOY_BYTES: usize = 25 * 1024 * 1024;
const MAX_FILES: usize = 1_000;

#[derive(Deserialize)]
struct DeployRequest {
    hostname: String,
    files: Vec<UploadedFile>,
}

#[derive(Deserialize)]
struct UploadedFile {
    path: String,
    content: String,
}

#[derive(Serialize)]
struct DeployResponse {
    url: String,
}

#[derive(Serialize)]
struct ErrorResponse<'a> {
    error: &'a str,
}

pub struct StaticHost {
    storage: SharedStorage,
    base_domain: String,
}

impl StaticHost {
    pub fn new(storage: SharedStorage, base_domain: String) -> Self {
        Self {
            storage,
            base_domain: base_domain.trim_end_matches('.').to_ascii_lowercase(),
        }
    }

    async fn static_response(&self, host: &str, request_path: &str) -> Response<Vec<u8>> {
        let Some(site) = site_from_host(host, &self.base_domain) else {
            return text_response(404, "No site matches this host. Use <site>.<base-domain>.");
        };
        let Some(relative_path) = safe_relative_path(request_path) else {
            return text_response(400, "Invalid path.");
        };

        for candidate in asset_candidates(&relative_path, request_path) {
            match self.storage.get(&site, &candidate).await {
                Ok(Some(object)) => return response(200, &object.content_type, object.body),
                Ok(None) => {}
                Err(_) => return text_response(500, "Could not read the requested asset."),
            }
        }
        text_response(404, "Site or asset not found.")
    }

    async fn deploy_response(
        &self,
        session: &mut ServerSession,
        request_host: &str,
    ) -> Response<Vec<u8>> {
        let body = match read_body(session).await {
            Ok(body) => body,
            Err(message) => return json_error(413, message),
        };
        let request: DeployRequest = match serde_json::from_slice(&body) {
            Ok(request) => request,
            Err(_) => return json_error(400, "Le corps JSON est invalide."),
        };

        if !valid_site_name(&request.hostname) {
            return json_error(
                400,
                "Le hostname doit contenir uniquement des lettres minuscules, chiffres et tirets.",
            );
        }
        if request.files.is_empty() {
            return json_error(400, "Aucun fichier reçu.");
        }
        if request.files.len() > MAX_FILES {
            return json_error(413, "Le déploiement dépasse la limite de 1 000 fichiers.");
        }

        let mut total_size = 0;
        let mut files = Vec::with_capacity(request.files.len());
        for file in request.files {
            if file.path.len() > 1_024 {
                return json_error(400, "Un chemin de fichier est trop long.");
            }
            let content = match base64::engine::general_purpose::STANDARD.decode(file.content) {
                Ok(content) => content,
                Err(_) => return json_error(400, "Le contenu d'un fichier est invalide."),
            };
            total_size += content.len();
            if total_size > MAX_DEPLOY_BYTES {
                return json_error(413, "Le déploiement dépasse la limite de 25 Mo.");
            }
            files.push((PathBuf::from(file.path), content));
        }

        let is_zip = files.len() == 1
            && files[0]
                .0
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("zip"));
        let files = if is_zip {
            let (_, archive) = files.pop().expect("one ZIP file");
            deploy::extract_zip_files(archive, MAX_FILES, MAX_DEPLOY_BYTES)
        } else {
            deploy::validate_files(&files).map(|_| files)
        };

        let files = match files {
            Ok(files) => files,
            Err(error) => return json_error_owned(400, error.to_string()),
        };
        if let Err(error) = self.storage.deploy(&request.hostname, files).await {
            return json_error_owned(500, error.to_string());
        };

        let port = request_host
            .rsplit_once(':')
            .and_then(|(_, port)| port.parse::<u16>().ok())
            .map(|port| format!(":{port}"))
            .unwrap_or_default();
        json_response(
            201,
            &DeployResponse {
                url: format!("http://{}.{}{}", request.hostname, self.base_domain, port),
            },
        )
    }
}

#[async_trait]
impl ServeHttp for StaticHost {
    async fn response(&self, session: &mut ServerSession) -> Response<Vec<u8>> {
        let method = session.req_header().method.clone();
        let request_path = session.req_header().uri.path().to_owned();
        let host = session
            .req_header()
            .headers
            .get("host")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_owned();

        if is_base_host(&host, &self.base_domain) {
            return match (method, request_path.as_str()) {
                (Method::GET, "/") => response(
                    200,
                    "text/html; charset=utf-8",
                    DASHBOARD.as_bytes().to_vec(),
                ),
                (Method::POST, "/api/deploy") => self.deploy_response(session, &host).await,
                _ => json_error(404, "Route introuvable."),
            };
        }

        if method != Method::GET && method != Method::HEAD {
            return text_response(405, "Method not allowed.");
        }
        self.static_response(&host, &request_path).await
    }
}

fn asset_candidates(relative_path: &Path, request_path: &str) -> Vec<PathBuf> {
    let mut candidates = vec![relative_path.to_path_buf()];
    if request_path.ends_with('/') && relative_path != Path::new("index.html") {
        candidates.push(relative_path.join("index.html"));
    } else if Path::new(request_path).extension().is_none() && request_path != "/" {
        candidates.push(relative_path.join("index.html"));
        candidates.push(PathBuf::from("index.html"));
    }
    candidates
}

async fn read_body(session: &mut ServerSession) -> Result<Vec<u8>, &'static str> {
    if session
        .get_header(http::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .is_some_and(|length| length > MAX_REQUEST_BYTES)
    {
        return Err("La requête dépasse la taille maximale autorisée.");
    }

    let mut body = Vec::new();
    loop {
        let chunk = session
            .read_request_body()
            .await
            .map_err(|_| "Impossible de lire la requête.")?;
        let Some(chunk) = chunk else {
            break;
        };
        if body.len() + chunk.len() > MAX_REQUEST_BYTES {
            session
                .drain_request_body()
                .await
                .map_err(|_| "Impossible de terminer la lecture de la requête.")?;
            return Err("La requête dépasse la taille maximale autorisée.");
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn response(status: u16, content_type: &str, body: Vec<u8>) -> Response<Vec<u8>> {
    Response::builder()
        .status(StatusCode::from_u16(status).expect("valid status code"))
        .header(http::header::CONTENT_TYPE, content_type)
        .header(http::header::CONTENT_LENGTH, body.len())
        .header("x-content-type-options", "nosniff")
        .header(http::header::CACHE_CONTROL, "no-cache")
        .body(body)
        .expect("valid HTTP response")
}

fn text_response(status: u16, message: &str) -> Response<Vec<u8>> {
    response(
        status,
        "text/plain; charset=utf-8",
        message.as_bytes().to_vec(),
    )
}

fn json_response<T: Serialize>(status: u16, value: &T) -> Response<Vec<u8>> {
    response(
        status,
        "application/json; charset=utf-8",
        serde_json::to_vec(value).expect("serializable JSON response"),
    )
}

fn json_error(status: u16, message: &'static str) -> Response<Vec<u8>> {
    json_response(status, &ErrorResponse { error: message })
}

fn json_error_owned(status: u16, message: String) -> Response<Vec<u8>> {
    response(
        status,
        "application/json; charset=utf-8",
        serde_json::to_vec(&serde_json::json!({ "error": message }))
            .expect("serializable JSON error"),
    )
}

pub fn valid_site_name(site: &str) -> bool {
    let bytes = site.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= 63
        && bytes[0].is_ascii_alphanumeric()
        && bytes[bytes.len() - 1].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
}

fn is_base_host(host: &str, base_domain: &str) -> bool {
    host_name(host).eq_ignore_ascii_case(base_domain)
}

fn host_name(host: &str) -> &str {
    host.trim()
        .trim_end_matches('.')
        .split(':')
        .next()
        .unwrap_or_default()
}

fn site_from_host(host: &str, base_domain: &str) -> Option<String> {
    let host = host_name(host).to_ascii_lowercase();
    let suffix = format!(".{base_domain}");
    let site = host.strip_suffix(&suffix)?;
    valid_site_name(site).then(|| site.to_owned())
}

fn safe_relative_path(request_path: &str) -> Option<PathBuf> {
    let path = request_path.trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    let decoded = percent_decode(path)?;
    let candidate = Path::new(&decoded);

    if candidate.components().all(|component| {
        matches!(component, Component::Normal(_))
            && component.as_os_str() != "."
            && component.as_os_str() != ".."
    }) {
        Some(candidate.to_path_buf())
    } else {
        None
    }
}

fn percent_decode(input: &str) -> Option<String> {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                let high = hex(bytes[index + 1])?;
                let low = hex(bytes[index + 2])?;
                output.push((high << 4) | low);
                index += 3;
            }
            b'%' => return None,
            byte => {
                output.push(byte);
                index += 1;
            }
        }
    }

    String::from_utf8(output).ok()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dashboard_is_compiled_into_the_application() {
        assert!(DASHBOARD.contains("quick-dashboard-bundle-v1"));
        assert!(!DASHBOARD.contains("@import"));
        assert!(!DASHBOARD.contains("<script src="));
        assert!(!DASHBOARD.contains("<link "));
        assert!(!DASHBOARD.contains("<img "));
    }

    #[test]
    fn extracts_site_from_host() {
        assert_eq!(
            site_from_host("demo.localhost:8080", "localhost"),
            Some("demo".to_owned())
        );
        assert_eq!(
            site_from_host("DEMO.LOCALHOST", "localhost"),
            Some("demo".to_owned())
        );
        assert_eq!(site_from_host("bad_name.localhost", "localhost"), None);
        assert_eq!(site_from_host("localhost", "localhost"), None);
        assert!(is_base_host("localhost:8080", "localhost"));
    }

    #[test]
    fn rejects_path_traversal() {
        assert!(safe_relative_path("/assets/app.js").is_some());
        assert!(safe_relative_path("/../secret").is_none());
        assert!(safe_relative_path("/%2e%2e/secret").is_none());
        assert!(safe_relative_path("/%ZZ").is_none());
    }

    #[test]
    fn builds_asset_candidates_for_directories_and_spa_routes() {
        assert_eq!(
            asset_candidates(Path::new("docs"), "/docs"),
            [
                PathBuf::from("docs"),
                PathBuf::from("docs/index.html"),
                PathBuf::from("index.html")
            ]
        );
        assert_eq!(
            asset_candidates(Path::new("assets/app.js"), "/assets/app.js"),
            [PathBuf::from("assets/app.js")]
        );
    }

    #[test]
    fn validates_dns_labels() {
        assert!(valid_site_name("demo"));
        assert!(valid_site_name("team-dashboard-2"));
        assert!(!valid_site_name(""));
        assert!(!valid_site_name("-demo"));
        assert!(!valid_site_name("Demo"));
    }
}
