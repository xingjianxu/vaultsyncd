//! Explicit optional Web resource assembly; source builds remain API-only by default.
use axum::{
    Router,
    http::{HeaderValue, header},
    middleware,
    response::Response,
};
use std::{
    io,
    path::{Path, PathBuf},
};
use tower_http::services::{ServeDir, ServeFile};

/// Deployment-owned directory; no build-time frontend embedding or implicit cwd lookup.
pub const WEB_DIR_ENV: &str = "VAULTSYNCD_WEB_DIR";
/// Validated static resources for the Web control plane.
pub struct WebAssets {
    directory: PathBuf,
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
        Ok(Self { directory })
    }
    /// Only root and /assets are public; unknown API paths never fall back to HTML.
    pub fn router(&self) -> Router {
        Router::new().route_service("/",ServeFile::new(self.directory.join("index.html")))
            .nest_service("/assets",ServeDir::new(self.directory.join("assets")).append_index_html_on_directories(false))
            .layer(middleware::map_response(|mut response:Response| async move{
                response.headers_mut().insert(header::X_CONTENT_TYPE_OPTIONS,HeaderValue::from_static("nosniff"));
                response.headers_mut().insert(header::CACHE_CONTROL,HeaderValue::from_static("no-store"));
                // React Aria 3.53.1 fixed pressable/iOS scroll styles; exact bytes are
                // checked by apps/web/tests/csp.mjs when dependencies change.
                response.headers_mut().insert(header::CONTENT_SECURITY_POLICY,HeaderValue::from_static("default-src 'self'; script-src 'self'; style-src 'self' 'sha256-38RhXrc7EdReTKsOm23ZPOCUgniTUUcjky8QOOrQx6o=' 'sha256-gYiS/BvZvRcK27JIXTuwhZ3hs2+VJ1X+2gUlE+farlg='; img-src 'self' data:; connect-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'"));
                response
            }))
    }
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
            "<script src=\"./assets/app.js\"></script>",
        )
        .unwrap();
        assert!(WebAssets::load(&root).is_err());
        std::fs::write(root.join("assets/app.js"), "console.log('packaged')").unwrap();
        assert!(WebAssets::load(&root).is_ok());
        std::fs::remove_dir_all(root).unwrap();
    }
}
