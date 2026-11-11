//! The admin UI is embedded so production needs neither Node nor a writable asset directory.
use axum::{
    Router,
    extract::OriginalUri,
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "admin/dist/"]
struct Assets;

pub fn routes() -> Router<crate::api::AppState> {
    Router::new()
        .route("/admin", get(asset))
        .route("/admin/", get(asset))
        .route("/admin/{*path}", get(asset))
}

async fn asset(OriginalUri(uri): OriginalUri) -> Response {
    response(uri.path())
}

fn response(path: &str) -> Response {
    let relative = path
        .strip_prefix("/admin")
        .unwrap_or("")
        .trim_start_matches('/');
    if relative == "api" || relative.starts_with("api/") || relative.contains("..") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let file = Assets::get(relative);
    let spa = relative.is_empty()
        || (!relative.contains('.')
            && matches!(
                relative.split('/').next(),
                Some(
                    "mailboxes"
                        | "credentials"
                        | "outgoing"
                        | "events"
                        | "logs"
                        | "system"
                        | "oauth"
                        | "login"
                )
            ));
    let (file, name) = match file {
        Some(file) => (file, relative),
        None if spa => match Assets::get("index.html") {
            Some(file) => (file, "index.html"),
            None => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
        },
        None => return StatusCode::NOT_FOUND.into_response(),
    };
    let content_type = match name.rsplit('.').next().unwrap_or("") {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "json" => "application/json",
        _ => "application/octet-stream",
    };
    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, if name.starts_with("assets/") { "public, max-age=31536000, immutable" } else { "no-cache" }),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (header::REFERRER_POLICY, "no-referrer"),
            (header::CONTENT_SECURITY_POLICY, "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; connect-src 'self'; frame-src 'self'; frame-ancestors 'none'; base-uri 'self'; form-action 'self'"),
        ],
        file.data.into_owned(),
    ).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_pages_work_but_api_and_missing_assets_do_not_fall_back() {
        for path in [
            "/admin",
            "/admin/",
            "/admin/mailboxes/example/messages",
            "/admin/oauth/callback",
        ] {
            let response = response(path);
            assert_eq!(response.status(), StatusCode::OK, "{path}");
            assert_eq!(
                response.headers()[header::CONTENT_TYPE],
                "text/html; charset=utf-8"
            );
            assert!(
                response
                    .headers()
                    .contains_key(header::CONTENT_SECURITY_POLICY)
            );
        }
        for path in [
            "/admin/api",
            "/admin/api/missing",
            "/admin/assets/missing.js",
            "/admin/unknown",
            "/admin/../config.yaml",
        ] {
            assert_eq!(response(path).status(), StatusCode::NOT_FOUND, "{path}");
        }
    }
}
