//! The viewer seam: a `/read/:id` page that wires `window.Shelfarr` (fileUrl +
//! server-backed progress) to whichever viewer plugin handles the book's format,
//! plus the `/progress/:id` store. The reader plugin writes zero backend code.

use crate::plugin::viewer_for;
use crate::{page, AppState};
use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    Json,
};
use maud::{html, PreEscaped};
use sqlx::Row;

/// GET /read/:id — render the viewer for this book.
pub async fn read(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    let row = sqlx::query("SELECT title, format FROM books WHERE id=?")
        .bind(id)
        .fetch_optional(&state.pool)
        .await
        .ok()
        .flatten();
    let Some(row) = row else {
        return (StatusCode::NOT_FOUND, "no such book").into_response();
    };
    let title: String = row.get("title");
    let format: String = row.get("format");

    let Some(viewer) = viewer_for(&state.plugins_dir, &format) else {
        let body = html! {
            section .empty {
                h1 { "No viewer for “" (format) "”" }
                p { "Install a viewer plugin that handles this format." }
            }
        };
        return Html(page(&title, body).into_string()).into_response();
    };
    let ui = viewer.ui.expect("viewer_for guarantees ui");

    // Server-inject the saved position so the viewer resumes without a round-trip.
    let saved: Option<String> = sqlx::query("SELECT position_json FROM progress WHERE user='default' AND book=?")
        .bind(id)
        .fetch_optional(&state.pool)
        .await
        .ok()
        .flatten()
        .map(|r| r.get("position_json"));
    let saved_js = saved.unwrap_or_else(|| "null".into());

    let bootstrap = format!(
        r#"window.Shelfarr = {{
  book: {{ id: {id}, format: {format:?}, fileUrl: "/books/{id}/file" }},
  saved: {saved_js},
  registerViewer(v) {{
    const el = document.getElementById('viewer');
    v.mount(el, {{
      fileUrl: this.book.fileUrl,
      progress: this.saved,
      onProgress: (p) => fetch('/progress/{id}', {{
        method: 'POST', headers: {{ 'Content-Type': 'application/json' }},
        body: JSON.stringify({{ viewer: v.id, position: p, percent: p.percent }})
      }})
    }});
  }}
}};"#
    );

    let css_href = ui.css.as_ref().map(|c| format!("/plugins/{}/{}", viewer.id, c));
    let js_src = format!("/plugins/{}/{}", viewer.id, ui.js);

    let body = html! {
        @if let Some(href) = &css_href { link rel="stylesheet" href=(href); }
        div .reader-wrap {
            div #viewer {}
            div .nav { button #prev { "‹" } button #next { "›" } }
        }
        script { (PreEscaped(bootstrap)) }
        script src=(js_src) {}
    };
    Html(page(&title, body).into_string()).into_response()
}

#[derive(serde::Deserialize)]
pub struct ProgressBody {
    pub viewer: String,
    pub position: serde_json::Value,
    pub percent: Option<f64>,
}

/// POST /progress/:id — upsert the reading position for the single user.
pub async fn save_progress(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(b): Json<ProgressBody>,
) -> Response {
    let pos = b.position.to_string();
    let res = sqlx::query(
        "INSERT INTO progress (user, book, viewer, position_json, percent, updated_at)
         VALUES ('default', ?, ?, ?, ?, datetime('now'))
         ON CONFLICT(user, book) DO UPDATE SET
           viewer=excluded.viewer, position_json=excluded.position_json,
           percent=excluded.percent, updated_at=datetime('now')",
    )
    .bind(id)
    .bind(&b.viewer)
    .bind(&pos)
    .bind(b.percent)
    .execute(&state.pool)
    .await;
    match res {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}
