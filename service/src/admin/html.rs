//! Server-rendered admin handlers and HTML templates for the built-in,
//! no-JavaScript admin portal (login form, dashboard, and key management).

use std::time::Instant;

use axum::{
    extract::{Form, Path, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
};
use chrono::Utc;

use crate::health::ServiceHealth;

use super::AdminState;
use super::dashboard::{DashboardData, dashboard_data};
use super::keys::{ApiKeyView, create_api_key, list_api_keys, revoke_api_key};
use super::login::{LoginForm, LoginResult, perform_login};
use super::session::{
    Authz, authorize, clear_cookie, csrf_token_for_request, csrf_valid, session_cookie,
    session_token_from_headers,
};

/// Hidden-field payload carrying the CSRF synchronizer token on POST forms.
#[derive(serde::Deserialize)]
pub(crate) struct CsrfForm {
    #[serde(default)]
    csrf_token: String,
}

fn forbidden() -> Response {
    (
        StatusCode::FORBIDDEN,
        Html("<h1>Invalid or missing CSRF token</h1>".to_string()),
    )
        .into_response()
}

pub(crate) async fn login_form() -> Html<String> {
    Html(render_login(None))
}

pub(crate) async fn login_submit(
    State(state): State<AdminState>,
    Form(form): Form<LoginForm>,
) -> Response {
    match perform_login(
        &state,
        &form.username,
        &form.password,
        Instant::now(),
        Utc::now(),
    )
    .await
    {
        Ok(LoginResult::Success { token }) => {
            let mut response = Redirect::to("/admin").into_response();
            if let Ok(cookie) =
                HeaderValue::from_str(&session_cookie(&token, state.cookie_secure()))
            {
                response.headers_mut().insert(header::SET_COOKIE, cookie);
            }
            response
        }
        Ok(LoginResult::Failed) => (
            StatusCode::UNAUTHORIZED,
            Html(render_login(Some("Invalid username or password."))),
        )
            .into_response(),
        Ok(LoginResult::LockedOut) => (
            StatusCode::UNAUTHORIZED,
            Html(render_login(Some(
                "Account temporarily locked due to repeated failed logins. Try again later.",
            ))),
        )
            .into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Html(render_login(Some(
                "A server error occurred. Please try again.",
            ))),
        )
            .into_response(),
    }
}

pub(crate) async fn logout(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> Response {
    let now = Instant::now();
    if authorize(&state, &headers, now) == Authz::Redirect {
        return Redirect::to("/admin/login").into_response();
    }
    if !csrf_valid(&state, &headers, &form.csrf_token, now) {
        return forbidden();
    }
    if let Some(token) = session_token_from_headers(&headers) {
        state
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&token);
    }
    let mut response = Redirect::to("/admin/login").into_response();
    if let Ok(cookie) = HeaderValue::from_str(&clear_cookie(state.cookie_secure())) {
        response.headers_mut().insert(header::SET_COOKIE, cookie);
    }
    response
}

pub(crate) async fn dashboard(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    let now = Instant::now();
    if authorize(&state, &headers, now) == Authz::Redirect {
        return Redirect::to("/admin/login").into_response();
    }
    let csrf = csrf_token_for_request(&state, &headers, now).unwrap_or_default();
    match dashboard_data(&state, Utc::now()).await {
        Ok(data) => Html(render_dashboard(&data, &csrf)).into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Html("<h1>Dashboard unavailable</h1>".to_string()),
        )
            .into_response(),
    }
}

pub(crate) async fn keys_view(State(state): State<AdminState>, headers: HeaderMap) -> Response {
    let now = Instant::now();
    if authorize(&state, &headers, now) == Authz::Redirect {
        return Redirect::to("/admin/login").into_response();
    }
    let csrf = csrf_token_for_request(&state, &headers, now).unwrap_or_default();
    match list_api_keys(&state).await {
        Ok(keys) => Html(render_keys(&keys, None, &csrf)).into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Html("<h1>Unable to load API keys</h1>".to_string()),
        )
            .into_response(),
    }
}

pub(crate) async fn keys_create(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> Response {
    let now = Instant::now();
    if authorize(&state, &headers, now) == Authz::Redirect {
        return Redirect::to("/admin/login").into_response();
    }
    if !csrf_valid(&state, &headers, &form.csrf_token, now) {
        return forbidden();
    }
    let csrf = csrf_token_for_request(&state, &headers, now).unwrap_or_default();
    match create_api_key(&state, Utc::now()).await {
        Ok((plaintext, _)) => match list_api_keys(&state).await {
            Ok(keys) => Html(render_keys(&keys, Some(&plaintext), &csrf)).into_response(),
            Err(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("<h1>Key created but listing failed</h1>".to_string()),
            )
                .into_response(),
        },
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Html("<h1>Unable to create API key</h1>".to_string()),
        )
            .into_response(),
    }
}

pub(crate) async fn keys_revoke(
    State(state): State<AdminState>,
    headers: HeaderMap,
    Path(id): Path<i64>,
    Form(form): Form<CsrfForm>,
) -> Response {
    let now = Instant::now();
    if authorize(&state, &headers, now) == Authz::Redirect {
        return Redirect::to("/admin/login").into_response();
    }
    if !csrf_valid(&state, &headers, &form.csrf_token, now) {
        return forbidden();
    }
    match revoke_api_key(&state, id, Utc::now()).await {
        Ok(_) => Redirect::to("/admin/keys").into_response(),
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Html("<h1>Unable to revoke API key</h1>".to_string()),
        )
            .into_response(),
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn render_login(error: Option<&str>) -> String {
    let banner = match error {
        Some(msg) => format!("<p class=\"error\">{}</p>", esc(msg)),
        None => String::new(),
    };
    format!(
        "<!DOCTYPE html><html><head><title>Admin Login</title></head><body>\
         <h1>Admin Login</h1>{banner}\
         <form method=\"post\" action=\"/admin/login\">\
         <label>Username <input type=\"text\" name=\"username\" autocomplete=\"username\"></label><br>\
         <label>Password <input type=\"password\" name=\"password\" autocomplete=\"current-password\"></label><br>\
         <button type=\"submit\">Sign in</button>\
         </form></body></html>"
    )
}

fn health_label(health: ServiceHealth) -> &'static str {
    match health {
        ServiceHealth::Healthy => "online",
        ServiceHealth::Degraded => "degraded",
        ServiceHealth::Unhealthy => "offline / error",
    }
}

fn csrf_input(csrf: &str) -> String {
    format!(
        "<input type=\"hidden\" name=\"csrf_token\" value=\"{}\">",
        esc(csrf)
    )
}

fn render_dashboard(data: &DashboardData, csrf: &str) -> String {
    let signal = match data.signal_percent {
        Some(p) => format!("{p}%"),
        None => "unavailable".to_string(),
    };
    let activity = if data.recent.is_empty() {
        "<li>No recent activity.</li>".to_string()
    } else {
        data.recent
            .iter()
            .map(|e| {
                format!(
                    "<li>{} — {}</li>",
                    esc(&e.timestamp.to_rfc3339()),
                    esc(&e.description)
                )
            })
            .collect::<String>()
    };
    format!(
        "<!DOCTYPE html><html><head><title>Admin Dashboard</title></head><body>\
         <h1>Dashboard</h1>\
         <p>Modem health: <strong>{}</strong></p>\
         <p>Signal quality: <strong>{signal}</strong></p>\
         <h2>Recent activity (last 24h)</h2><ul>{activity}</ul>\
         <p><a href=\"/admin/keys\">Manage API keys</a></p>\
         <form method=\"post\" action=\"/admin/logout\">{logout_csrf}<button type=\"submit\">Sign out</button></form>\
         </body></html>",
        health_label(data.health),
        logout_csrf = csrf_input(csrf)
    )
}

fn render_keys(keys: &[ApiKeyView], new_key: Option<&str>, csrf: &str) -> String {
    let banner = match new_key {
        Some(key) => format!(
            "<p class=\"new-key\">New API key (copy it now, it will not be shown again): \
             <code>{}</code></p>",
            esc(key)
        ),
        None => String::new(),
    };
    let rows = if keys.is_empty() {
        "<tr><td colspan=\"4\">No API keys.</td></tr>".to_string()
    } else {
        keys.iter()
            .map(|k| {
                let revoke_cell = if k.revoked {
                    "revoked".to_string()
                } else {
                    format!(
                        "<form method=\"post\" action=\"/admin/keys/{}/revoke\">{}\
                         <button type=\"submit\">Revoke</button></form>",
                        k.id,
                        csrf_input(csrf)
                    )
                };
                format!(
                    "<tr><td>{}</td><td><code>{}</code></td><td>{}</td><td>{}</td></tr>",
                    k.id,
                    esc(&k.key_identifier),
                    esc(&k.created_at.to_rfc3339()),
                    revoke_cell
                )
            })
            .collect::<String>()
    };
    format!(
        "<!DOCTYPE html><html><head><title>API Keys</title></head><body>\
         <h1>API Keys</h1>{banner}\
         <form method=\"post\" action=\"/admin/keys\">{create_csrf}<button type=\"submit\">Create new key</button></form>\
         <table><thead><tr><th>ID</th><th>Identifier</th><th>Created</th><th>Action</th></tr></thead>\
         <tbody>{rows}</tbody></table>\
         <p><a href=\"/admin\">Back to dashboard</a></p>\
         </body></html>",
        create_csrf = csrf_input(csrf)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    #[test]
    fn rendered_views_escape_dynamic_values() {
        let keys = vec![ApiKeyView {
            id: 1,
            key_identifier: "<script>".to_string(),
            custom_rate_limit: None,
            revoked: false,
            created_at: Utc::now(),
        }];
        let html = render_keys(&keys, Some("<b>plain</b>"), "tok");
        assert!(!html.contains("<script>"));
        assert!(html.contains("&lt;script&gt;"));
    }
}
