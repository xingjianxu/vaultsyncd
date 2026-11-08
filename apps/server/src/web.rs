//! Explicit optional Web resource assembly; source builds remain API-only by default.
use axum::{
    Router,
    http::{HeaderValue, StatusCode, header},
    middleware,
    response::{Html, IntoResponse, Response},
    routing::get,
};
use rand_core::{OsRng, RngCore};
use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
};
use tower_http::services::ServeDir;

/// Deployment-owned directory; no build-time frontend embedding or implicit cwd lookup.
pub const WEB_DIR_ENV: &str = "VAULTSYNCD_WEB_DIR";
/// A fresh nonce permits Mantine's generated style elements, never inline scripts.
/// Browser/CSP tests read this single policy template; only the nonce is replaced.
pub const WEB_CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'nonce-__STYLE_NONCE__'; img-src 'self' data:; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'";
/// Validated static resources for the Web control plane.
pub struct WebAssets {
    directory: PathBuf,
    index: Arc<str>,
}
impl WebAssets {
    /// Read explicit environment configuration; absent means API-only.
    pub fn configured() -> io::Result<Option<Self>> {
        std::env::var_os(WEB_DIR_ENV)
            .map(|path| Self::load(Path::new(&path)))
            .transpose()
    }
    /// Validate index and its generated asset references before opening the database.
    pub fn load(path: &Path) -> io::Result<Self> {
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "VAULTSYNCD_WEB_DIR must be absolute",
            ));
        }
        let directory = path.canonicalize()?;
        let index = std::fs::read_to_string(directory.join("index.html"))?;
        if !directory.join("assets").is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "Web assets directory missing",
            ));
        }
        let mut references = 0;
        for marker in ["src=\"", "href=\""] {
            for part in index.split(marker).skip(1) {
                let value = part
                    .split('"')
                    .next()
                    .unwrap_or_default()
                    .trim_start_matches("./");
                if !value.starts_with("assets/") {
                    continue;
                }
                if Path::new(value)
                    .components()
                    .any(|part| !matches!(part, std::path::Component::Normal(_)))
                    || !directory.join(value).is_file()
                {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        format!("Web resource missing or invalid: {value}"),
                    ));
                }
                references += 1;
            }
        }
        if references == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Web index has no packaged assets",
            ));
        }
        if !index.contains("</head>") {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Web index head missing",
            ));
        }
        Ok(Self {
            directory,
            index: index.into(),
        })
    }
    /// Only root and /assets are public; unknown API paths never fall back to HTML.
    pub fn router(&self) -> Router {
        let index = Arc::clone(&self.index);
        Router::new().route("/", get(move || {
            let index = Arc::clone(&index);
            async move { index_response(&index) }
        }))
            .nest_service("/assets",ServeDir::new(self.directory.join("assets")).append_index_html_on_directories(false))
            .layer(middleware::map_response(|mut response:Response| async move{
                response.headers_mut().insert(header::X_CONTENT_TYPE_OPTIONS,HeaderValue::from_static("nosniff"));
                response.headers_mut().insert(header::CACHE_CONTROL,HeaderValue::from_static("no-store"));
                // HTML already carries its per-response nonce. Static resources
                // have no style elements and use the same policy without a nonce.
                response.headers_mut().entry(header::CONTENT_SECURITY_POLICY).or_insert_with(||
                    HeaderValue::from_static("default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'"));
                response
            }))
    }
}

fn index_response(index: &str) -> Result<Response, StatusCode> {
    let mut bytes = [0u8; 16];
    OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let nonce: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    let meta = format!("<meta name=\"csp-nonce\" content=\"{nonce}\"></head>");
    let mut response = Html(index.replacen("</head>", &meta, 1)).into_response();
    let policy = HeaderValue::from_str(&WEB_CSP.replace("__STYLE_NONCE__", &nonce))
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    response
        .headers_mut()
        .insert(header::CONTENT_SECURITY_POLICY, policy);
    Ok(response)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn absent_or_missing_resources_do_not_become_empty_success() {
        assert!(WebAssets::load(Path::new("relative")).is_err());
        let root = std::env::temp_dir().join(format!(
            "vs-web-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(root.join("assets")).unwrap();
        std::fs::write(
            root.join("index.html"),
            "<html><head><script src=\"./assets/app.js\"></script></head><body></body></html>",
        )
        .unwrap();
        assert!(WebAssets::load(&root).is_err());
        std::fs::write(root.join("assets/app.js"), "console.log('packaged')").unwrap();
        assert!(WebAssets::load(&root).is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn html_nonces_match_the_policy_and_change_per_response() {
        use axum::{
            body::{Body, to_bytes},
            http::Request,
        };
        use tower::ServiceExt;

        let index = "<!doctype html><html><head></head><body><script src=\"/assets/app.js\"></script></body></html>";
        let web = WebAssets {
            directory: std::env::temp_dir(),
            index: index.into(),
        };
        let router = web.router();
        let mut previous = None;
        for _ in 0..2 {
            let response = router
                .clone()
                .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            assert_eq!(
                response.headers()[header::X_CONTENT_TYPE_OPTIONS],
                "nosniff"
            );
            let policy = response.headers()[header::CONTENT_SECURITY_POLICY]
                .to_str()
                .unwrap()
                .to_owned();
            assert!(policy.contains("script-src 'self';"));
            assert!(!policy.contains("unsafe-inline"));
            let body = to_bytes(response.into_body(), 8192).await.unwrap();
            let html = std::str::from_utf8(&body).unwrap();
            let nonce = html
                .split("name=\"csp-nonce\" content=\"")
                .nth(1)
                .unwrap()
                .split('"')
                .next()
                .unwrap();
            assert_eq!(nonce.len(), 32);
            assert!(nonce.bytes().all(|byte| byte.is_ascii_hexdigit()));
            assert!(policy.contains(&format!("'nonce-{nonce}'")));
            assert!(!html.contains("<script nonce="));
            assert_ne!(previous.as_deref(), Some(nonce));
            previous = Some(nonce.to_owned());
        }
        let missing = router
            .oneshot(
                Request::builder()
                    .uri("/v1/missing")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    }
}
