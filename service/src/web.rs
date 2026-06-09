
use std::path::PathBuf;

use axum::Router;
use tower_http::services::{ServeDir, ServeFile};

const WEB_UI_DIR_ENV: &str = "SMS_WEB_UI_DIR";

const DEFAULT_WEB_UI_DIRNAME: &str = "web-ui";

fn assets_dir() -> PathBuf {
    if let Ok(dir) = std::env::var(WEB_UI_DIR_ENV) {
        return PathBuf::from(dir);
    }

    if let Ok(exe) = std::env::current_exe()
        && let Some(parent) = exe.parent()
    {
        return parent.join(DEFAULT_WEB_UI_DIRNAME);
    }

    PathBuf::from(DEFAULT_WEB_UI_DIRNAME)
}

/// Creates a router for serving the web UI assets.
pub fn router() -> Router {
    let dir = assets_dir();
    let index = dir.join("index.html");
    let service = ServeDir::new(dir).fallback(ServeFile::new(index));
    Router::new().fallback_service(service)
}
