//! Static web UI serving, gated behind the `web-ui` cargo feature.
//!
//! The admin front-end lives under `web-ui/` (Next.js + React + TypeScript) and
//! is statically exported to `web-ui/out` with Bun by the build script. This
//! module wires those exported assets into the Axum router as a static-file
//! fallback so the service serves the portal without inlining HTML in source.
//!
//! At runtime the assets directory is resolved from the `SMS_WEB_UI_DIR`
//! environment variable when set, otherwise from a `web-ui` directory beside
//! the executable — matching the `dist/` layout the build assembles (the
//! binary and the `web-ui/` assets sit side by side).

use std::path::PathBuf;

use axum::Router;
use tower_http::services::{ServeDir, ServeFile};

/// Environment variable overriding the exported-assets directory.
const WEB_UI_DIR_ENV: &str = "SMS_WEB_UI_DIR";

/// Default assets directory name, resolved next to the executable.
const DEFAULT_WEB_UI_DIRNAME: &str = "web-ui";

/// Resolve the directory holding the exported static assets.
///
/// Precedence: the `SMS_WEB_UI_DIR` override, then `web-ui` beside the running
/// executable, then a bare `web-ui` relative to the working directory.
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

/// Build the router that serves the bundled web UI.
///
/// Returns a router whose fallback serves the exported static assets, with the
/// site's `index.html` as the fallback for unmatched paths. Callers merge it
/// into the main router so API and admin routes still take precedence.
pub fn router() -> Router {
    let dir = assets_dir();
    let index = dir.join("index.html");
    let service = ServeDir::new(dir).fallback(ServeFile::new(index));
    Router::new().fallback_service(service)
}
