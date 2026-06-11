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
                Ok(Some(object)) if is_jsx(&candidate) => {
                    return jsx_response(&candidate, &object.body);
                }
                Ok(Some(object)) => return response(200, &object.content_type, object.body),
                Ok(None) => {}
                Err(_) => return text_response(500, "Could not read the requested asset."),
            }
        }
        if request_path == "/" {
            return match self
                .storage
                .get(&site, Path::new(deploy::MANIFEST_PATH))
                .await
            {
                Ok(Some(object)) => directory_listing_response(&site, &object.body),
                Ok(None) => text_response(404, "Site or asset not found."),
                Err(_) => text_response(500, "Could not read the site manifest."),
            };
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
        let accept_language = session
            .req_header()
            .headers
            .get(http::header::ACCEPT_LANGUAGE)
            .and_then(|value| value.to_str().ok());

        if is_base_host(&host, &self.base_domain) {
            return match (method, request_path.as_str()) {
                (Method::GET, "/") => dashboard_response(accept_language),
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

fn dashboard_response(accept_language: Option<&str>) -> Response<Vec<u8>> {
    let language = preferred_dashboard_language(accept_language);
    let body = DASHBOARD.replacen(
        r#"<meta name="quick-language" content="en">"#,
        &format!(r#"<meta name="quick-language" content="{language}">"#),
        1,
    );
    Response::builder()
        .status(StatusCode::OK)
        .header(http::header::CONTENT_TYPE, "text/html; charset=utf-8")
        .header(http::header::CONTENT_LENGTH, body.len())
        .header(http::header::VARY, http::header::ACCEPT_LANGUAGE.as_str())
        .header("x-content-type-options", "nosniff")
        .header(http::header::CACHE_CONTROL, "no-cache")
        .body(body.into_bytes())
        .expect("valid dashboard response")
}

fn preferred_dashboard_language(accept_language: Option<&str>) -> &'static str {
    let mut preferences: Vec<_> = accept_language
        .unwrap_or_default()
        .split(',')
        .enumerate()
        .filter_map(|(position, item)| {
            let mut parts = item.trim().split(';');
            let tag = parts.next()?.trim();
            let quality = parts
                .find_map(|parameter| parameter.trim().strip_prefix("q="))
                .and_then(|value| value.parse::<f32>().ok())
                .unwrap_or(1.0);
            Some((quality, position, tag))
        })
        .collect();
    preferences.sort_by(|left, right| {
        right
            .0
            .total_cmp(&left.0)
            .then_with(|| left.1.cmp(&right.1))
    });

    for (quality, _, tag) in preferences {
        if quality <= 0.0 {
            continue;
        }
        let primary = tag.split('-').next().unwrap_or_default();
        if primary.eq_ignore_ascii_case("fr") {
            return "fr";
        }
        if primary.eq_ignore_ascii_case("en") || tag == "*" {
            return "en";
        }
    }
    "en"
}

fn is_jsx(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("jsx"))
}

fn directory_listing_response(site: &str, manifest: &[u8]) -> Response<Vec<u8>> {
    let manifest: deploy::SiteManifest = match serde_json::from_slice(manifest) {
        Ok(manifest) => manifest,
        Err(_) => return text_response(500, "The site manifest is invalid."),
    };
    let mut items = String::new();
    for path in manifest.files {
        items.push_str("<li><a href=\"/");
        items.push_str(&percent_encode_path(&path));
        items.push_str("\">");
        items.push_str(&escape_html(&path));
        items.push_str("</a></li>");
    }
    let body = format!(
        r##"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>{site} - files</title>
  <style>
    :root {{ color-scheme: light dark; font-family: ui-monospace, SFMono-Regular, Consolas, monospace; }}
    body {{ max-width: 920px; margin: 0 auto; padding: 48px 24px; }}
    h1 {{ font: 700 clamp(32px, 6vw, 64px) system-ui, sans-serif; letter-spacing: -.05em; }}
    p {{ opacity: .65; }}
    ul {{ padding: 0; list-style: none; border-top: 1px solid color-mix(in srgb, currentColor 20%, transparent); }}
    li {{ border-bottom: 1px solid color-mix(in srgb, currentColor 20%, transparent); }}
    a {{ display: block; padding: 14px 4px; color: inherit; text-decoration: none; }}
    a:hover {{ padding-left: 12px; color: #7357ff; }}
  </style>
</head>
<body>
  <p>Quick file listing</p>
  <h1>{site}</h1>
  <ul>{items}</ul>
</body>
</html>"##,
        site = escape_html(site),
    );
    response(200, "text/html; charset=utf-8", body.into_bytes())
}

fn jsx_response(path: &Path, source: &[u8]) -> Response<Vec<u8>> {
    let encoded = base64::engine::general_purpose::STANDARD.encode(source);
    let file_name = path.to_string_lossy();
    let file_name_html = escape_html(&file_name);
    let file_name_js = serde_json::to_string(file_name.as_ref()).expect("serializable file name");
    let body = format!(
        r##"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>{file_name_html}</title>
  <script type="importmap">
    {{"imports":{{"react":"https://esm.sh/react@18.3.1","react/jsx-runtime":"https://esm.sh/react@18.3.1/jsx-runtime","react/jsx-dev-runtime":"https://esm.sh/react@18.3.1/jsx-dev-runtime","react-dom":"https://esm.sh/react-dom@18.3.1?external=react","react-dom/client":"https://esm.sh/react-dom@18.3.1/client?external=react","react-dom/server":"https://esm.sh/react-dom@18.3.1/server?external=react"}}}}
  </script>
  <script src="https://unpkg.com/@babel/standalone@7/babel.min.js"></script>
  <style>
    html, body, #root {{ min-height: 100%; margin: 0; }}
    #quick-error {{ margin: 0; padding: 24px; color: #b42318; white-space: pre-wrap; font: 14px/1.5 ui-monospace, monospace; }}
  </style>
</head>
<body>
  <div id="root"></div>
  <pre id="quick-error" hidden></pre>
  <script type="module">
    const errorNode = document.querySelector("#quick-error");
    try {{
      const bytes = Uint8Array.from(atob("{encoded}"), character => character.charCodeAt(0));
      const source = new TextDecoder().decode(bytes);
      const hasDefaultExport = /\bexport\s+default\b/.test(source);
      const moduleSource = hasDefaultExport
        ? source
        : `export default function QuickEntry() {{ return (${{source}}); }}`;
      const pinnedModules = new Map([
        ["react", "https://esm.sh/react@18.3.1"],
        ["react/jsx-runtime", "https://esm.sh/react@18.3.1/jsx-runtime"],
        ["react/jsx-dev-runtime", "https://esm.sh/react@18.3.1/jsx-dev-runtime"],
        ["react-dom", "https://esm.sh/react-dom@18.3.1?external=react"],
        ["react-dom/client", "https://esm.sh/react-dom@18.3.1/client?external=react"],
        ["react-dom/server", "https://esm.sh/react-dom@18.3.1/server?external=react"]
      ]);
      const isBareModule = specifier =>
        !specifier.startsWith(".") &&
        !specifier.startsWith("/") &&
        !specifier.startsWith("#") &&
        !/^[a-zA-Z][a-zA-Z\d+.-]*:/.test(specifier);
      const resolveModule = specifier => {{
        const pinned = pinnedModules.get(specifier);
        if (pinned) return pinned;
        if (!isBareModule(specifier)) return specifier;
        const separator = specifier.includes("?") ? "&" : "?";
        return `https://esm.sh/${{specifier}}${{separator}}external=react,react-dom`;
      }};
      const resolveImports = ({{ types }}) => ({{
        visitor: {{
          ImportDeclaration(path) {{
            path.node.source.value = resolveModule(path.node.source.value);
          }},
          ExportNamedDeclaration(path) {{
            if (path.node.source) path.node.source.value = resolveModule(path.node.source.value);
          }},
          ExportAllDeclaration(path) {{
            path.node.source.value = resolveModule(path.node.source.value);
          }},
          CallExpression(path) {{
            if (
              path.node.callee.type === "Import" &&
              path.node.arguments.length === 1 &&
              types.isStringLiteral(path.node.arguments[0])
            ) {{
              path.node.arguments[0].value = resolveModule(path.node.arguments[0].value);
            }}
          }}
        }}
      }});
      const transformed = Babel.transform(moduleSource, {{
        filename: {file_name_js},
        sourceType: "module",
        plugins: [resolveImports],
        presets: [["react", {{ runtime: "automatic" }}]]
      }}).code;
      const moduleUrl = URL.createObjectURL(new Blob([transformed], {{ type: "text/javascript" }}));
      const [entry, React, ReactDOM] = await Promise.all([
        import(moduleUrl),
        import("react"),
        import("react-dom/client")
      ]);
      if (!entry.default) throw new Error("The JSX file must export a default React component.");
      ReactDOM.createRoot(document.querySelector("#root")).render(React.createElement(entry.default));
      URL.revokeObjectURL(moduleUrl);
    }} catch (error) {{
      errorNode.hidden = false;
      errorNode.textContent = "Quick could not render " + {file_name_js} + "\n\n" + (error.stack || error);
    }}
  </script>
</body>
</html>"##
    );
    response(200, "text/html; charset=utf-8", body.into_bytes())
}

fn escape_html(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

fn percent_encode_path(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    for byte in input.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~' | b'/') {
            output.push(byte as char);
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    output
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
        assert!(DASHBOARD.contains(r#"<html lang="en">"#));
        assert!(DASHBOARD.contains("ready to publish"));
        assert!(DASHBOARD.contains(r#"data-language="fr""#));
        assert!(!DASHBOARD.contains("@import"));
        assert!(!DASHBOARD.contains("<script src="));
        assert!(!DASHBOARD.contains("<link "));
        assert!(!DASHBOARD.contains("<img "));
    }

    #[test]
    fn selects_dashboard_language_from_accept_language() {
        assert_eq!(preferred_dashboard_language(Some("fr-BE,fr;q=0.9")), "fr");
        assert_eq!(
            preferred_dashboard_language(Some("en-US;q=0.8,fr-FR;q=0.9")),
            "fr"
        );
        assert_eq!(preferred_dashboard_language(Some("fr;q=0,en;q=0.8")), "en");
        assert_eq!(preferred_dashboard_language(Some("de-DE,de;q=0.9")), "en");
        assert_eq!(preferred_dashboard_language(None), "en");
    }

    #[test]
    fn dashboard_response_exposes_the_requested_language() {
        let response = dashboard_response(Some("fr-BE,fr;q=0.9"));
        assert_eq!(
            response.headers().get(http::header::VARY).unwrap(),
            "accept-language"
        );
        let body = String::from_utf8(response.into_body()).unwrap();
        assert!(body.contains(r#"<meta name="quick-language" content="fr">"#));
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
    fn builds_a_safe_directory_listing() {
        let manifest = serde_json::to_vec(&deploy::SiteManifest {
            files: vec!["App.jsx".to_owned(), "notes & docs/read me.md".to_owned()],
        })
        .unwrap();
        let response = directory_listing_response("demo", &manifest);
        let body = String::from_utf8(response.into_body()).unwrap();

        assert!(body.contains("href=\"/App.jsx\""));
        assert!(body.contains("href=\"/notes%20%26%20docs/read%20me.md\""));
        assert!(body.contains("notes &amp; docs/read me.md"));
    }

    #[test]
    fn wraps_jsx_as_an_executable_react_page() {
        let response = jsx_response(Path::new("App.jsx"), b"export default () => <h1>Hello</h1>");
        let body = String::from_utf8(response.into_body()).unwrap();

        assert!(body.contains("@babel/standalone"));
        assert!(body.contains("react-dom/client"));
        assert!(body.contains("plugins: [resolveImports]"));
        assert!(body.contains("https://esm.sh/${specifier}"));
        assert!(body.contains("ImportDeclaration(path)"));
        assert!(body.contains("ExportAllDeclaration(path)"));
        assert!(body.contains(r#"path.node.callee.type === "Import""#));
        assert!(body.contains("ZXhwb3J0IGRlZmF1bHQ"));
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
