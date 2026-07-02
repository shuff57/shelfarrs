//! Auth + multi-user. argon2-hashed passwords, server-side sessions (cookie), and
//! a gate that also accepts HTTP Basic so OPDS readers can authenticate. The library
//! is shared; only login and reading progress are per-user.

use crate::{page, AppState};
use argon2::password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::{Html, IntoResponse, Redirect, Response},
    Form,
};
use base64::Engine;
use maud::html;
use rand::RngCore;
use serde::Deserialize;
use sqlx::{Row, SqlitePool};

/// The authenticated user, injected into request extensions by the gate.
#[derive(Clone)]
pub struct CurrentUser(pub String);

fn hash_password(pw: &str) -> anyhow::Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(pw.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| anyhow::anyhow!("hash: {e}"))
}

fn verify_password(pw: &str, hash: &str) -> bool {
    PasswordHash::new(hash)
        .map(|h| Argon2::default().verify_password(pw.as_bytes(), &h).is_ok())
        .unwrap_or(false)
}

/// Create the admin account on first run. Password from SHELFARRS_ADMIN_PASSWORD,
/// else "admin" with a loud warning.
pub async fn bootstrap_admin(pool: &SqlitePool) -> anyhow::Result<()> {
    let n: i64 = sqlx::query("SELECT COUNT(*) AS c FROM users")
        .fetch_one(pool)
        .await?
        .get("c");
    if n > 0 {
        return Ok(());
    }
    let pw = std::env::var("SHELFARRS_ADMIN_PASSWORD").unwrap_or_else(|_| {
        tracing::warn!("no SHELFARRS_ADMIN_PASSWORD set — bootstrapping admin/admin, CHANGE IT");
        "admin".into()
    });
    sqlx::query("INSERT INTO users (username, password_hash, is_admin) VALUES ('admin', ?, 1)")
        .bind(hash_password(&pw)?)
        .execute(pool)
        .await?;
    tracing::info!("created default admin user");
    Ok(())
}

fn new_token() -> String {
    let mut b = [0u8; 32];
    OsRng.fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

async fn user_for_token(pool: &SqlitePool, token: &str) -> Option<String> {
    sqlx::query("SELECT username FROM sessions WHERE token=?")
        .bind(token)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .map(|r| r.get("username"))
}

fn cookie_token(req: &Request) -> Option<String> {
    let cookies = req.headers().get(header::COOKIE)?.to_str().ok()?;
    cookies.split(';').find_map(|c| {
        let (k, v) = c.trim().split_once('=')?;
        (k == "session").then(|| v.to_string())
    })
}

async fn basic_auth(pool: &SqlitePool, header: Option<String>) -> Option<String> {
    let h = header?;
    let b64 = h.strip_prefix("Basic ")?;
    let decoded = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    let s = String::from_utf8(decoded).ok()?;
    let (user, pass) = s.split_once(':')?;
    let hash: String = sqlx::query("SELECT password_hash FROM users WHERE username=?")
        .bind(user)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .map(|r| r.get("password_hash"))?;
    verify_password(pass, &hash).then(|| user.to_string())
}

/// The gate. Exempts /login, /assets, /healthz; accepts session cookie or HTTP Basic.
pub async fn gate(state: AppState, mut req: Request, next: Next) -> Response {
    let path = req.uri().path().to_string();
    if path == "/login" || path == "/healthz" || path.starts_with("/assets") {
        return next.run(req).await;
    }

    // Pull what we need out of the request BEFORE any await (Request isn't Sync).
    let token = cookie_token(&req);
    let auth_header = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let wants_html = req
        .headers()
        .get(header::ACCEPT)
        .and_then(|a| a.to_str().ok())
        .is_some_and(|a| a.contains("text/html"));

    let user = match token {
        Some(t) => user_for_token(&state.pool, &t).await,
        None => None,
    };
    let user = match user {
        Some(u) => Some(u),
        None => basic_auth(&state.pool, auth_header).await,
    };

    match user {
        Some(u) => {
            req.extensions_mut().insert(CurrentUser(u));
            next.run(req).await
        }
        None => {
            if wants_html {
                Redirect::to("/login").into_response()
            } else {
                (
                    StatusCode::UNAUTHORIZED,
                    [(header::WWW_AUTHENTICATE, "Basic realm=\"shelfarrs\"")],
                    "authentication required",
                )
                    .into_response()
            }
        }
    }
}

// ---- handlers ----

pub async fn login_page() -> Html<String> {
    let body = html! {
        section .login {
            h1 { "Sign in" }
            form action="/login" method="post" .search style="flex-direction:column;align-items:stretch;max-width:320px" {
                input type="text" name="username" placeholder="Username" autofocus required;
                input type="password" name="password" placeholder="Password" required style="margin-top:.5rem";
                button .btn type="submit" style="margin-top:.75rem" { "Sign in" }
            }
        }
    };
    Html(page("Sign in", body).into_string())
}

#[derive(Deserialize)]
pub struct LoginForm {
    pub username: String,
    pub password: String,
}

pub async fn login(State(state): State<AppState>, Form(f): Form<LoginForm>) -> Response {
    let hash: Option<String> = sqlx::query("SELECT password_hash FROM users WHERE username=?")
        .bind(&f.username)
        .fetch_optional(&state.pool)
        .await
        .ok()
        .flatten()
        .map(|r| r.get("password_hash"));
    if !hash.map(|h| verify_password(&f.password, &h)).unwrap_or(false) {
        let body = html! { section .empty { h1 { "Wrong username or password" } p { a href="/login" { "Try again" } } } };
        return (StatusCode::UNAUTHORIZED, Html(page("Sign in", body).into_string())).into_response();
    }
    let token = new_token();
    let _ = sqlx::query("INSERT INTO sessions (token, username) VALUES (?, ?)")
        .bind(&token)
        .bind(&f.username)
        .execute(&state.pool)
        .await;
    (
        [(
            header::SET_COOKIE,
            format!("session={token}; HttpOnly; Path=/; SameSite=Lax; Max-Age=2592000"),
        )],
        Redirect::to("/"),
    )
        .into_response()
}

pub async fn logout(State(state): State<AppState>, req: Request) -> Response {
    if let Some(t) = cookie_token(&req) {
        let _ = sqlx::query("DELETE FROM sessions WHERE token=?").bind(t).execute(&state.pool).await;
    }
    (
        [(header::SET_COOKIE, "session=; HttpOnly; Path=/; Max-Age=0")],
        Redirect::to("/login"),
    )
        .into_response()
}

// ---- admin: user management ----

pub async fn users_page(State(state): State<AppState>) -> Html<String> {
    let rows = sqlx::query("SELECT username, is_admin FROM users ORDER BY username")
        .fetch_all(&state.pool)
        .await
        .unwrap_or_default();
    let body = html! {
        h1 { "Users" }
        div .results {
            @for r in &rows {
                @let name: String = r.get("username");
                @let admin: i64 = r.get("is_admin");
                div .result {
                    span .title { (name) @if admin == 1 { " " span .fmt { "admin" } } }
                    @if name != "admin" {
                        form .inline hx-post="/settings/users/remove" hx-swap="none" {
                            input type="hidden" name="username" value=(name);
                            button .link { "remove" }
                        }
                    }
                }
            }
        }
        h2 { "Add user" }
        form .search action="/settings/users/add" method="post" {
            input type="text" name="username" placeholder="Username" required;
            input type="password" name="password" placeholder="Password" required;
            button .btn type="submit" { "Create" }
        }
        p style="margin-top:1rem" { a href="/logout" { "Sign out" } }
    };
    Html(page("Users", body).into_string())
}

pub async fn user_add(State(state): State<AppState>, Form(f): Form<LoginForm>) -> Response {
    let u = f.username.trim();
    if u.is_empty() || f.password.is_empty() {
        return Redirect::to("/settings/users").into_response();
    }
    match hash_password(&f.password) {
        Ok(h) => {
            let _ = sqlx::query("INSERT OR IGNORE INTO users (username, password_hash) VALUES (?, ?)")
                .bind(u)
                .bind(h)
                .execute(&state.pool)
                .await;
        }
        Err(e) => tracing::error!("hash failed: {e}"),
    }
    Redirect::to("/settings/users").into_response()
}

#[derive(Deserialize)]
pub struct UserForm {
    pub username: String,
}

pub async fn user_remove(State(state): State<AppState>, Form(f): Form<UserForm>) -> Response {
    if f.username != "admin" {
        let _ = sqlx::query("DELETE FROM users WHERE username=?").bind(&f.username).execute(&state.pool).await;
        let _ = sqlx::query("DELETE FROM sessions WHERE username=?").bind(&f.username).execute(&state.pool).await;
    }
    (StatusCode::OK, "removed").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_roundtrip() {
        let h = hash_password("s3cret").unwrap();
        assert!(verify_password("s3cret", &h));
        assert!(!verify_password("wrong", &h));
        assert!(h.starts_with("$argon2"));
    }
}
