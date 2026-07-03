//! Discovery: followed authors + an import-list. Relational-ish, but at homelab
//! scale it's a JSON array in `settings` (per the design's ponytail call) — no tables.

use crate::source::{Candidate, SearchQuery};
use crate::{page, AppState};
use anyhow::Result;
use axum::{
    extract::State,
    response::{Html, IntoResponse, Redirect, Response},
    Form,
};
use maud::html;
use serde::Deserialize;
use sqlx::Row;

const FOLLOWS_KEY: &str = "follows";

async fn get_json<T: serde::de::DeserializeOwned + Default>(state: &AppState, key: &str) -> T {
    let raw: Option<String> = sqlx::query("SELECT value FROM settings WHERE key=?")
        .bind(key)
        .fetch_optional(&state.pool)
        .await
        .ok()
        .flatten()
        .map(|r| r.get("value"));
    raw.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

async fn set_json<T: serde::Serialize>(state: &AppState, key: &str, val: &T) -> Result<()> {
    sqlx::query("INSERT INTO settings (key,value) VALUES (?,?) ON CONFLICT(key) DO UPDATE SET value=excluded.value")
        .bind(key)
        .bind(serde_json::to_string(val)?)
        .execute(&state.pool)
        .await?;
    Ok(())
}

/// Search every source, import the best match (prefer epub). Powers import-lists.
pub async fn acquire(state: &AppState, query: &str) -> Result<()> {
    let sources = state.sources();
    let sq = SearchQuery { text: query.to_string(), format: None };
    let mut best: Option<Candidate> = None;
    for src in sources.values() {
        if let Ok(cands) = src.search(&sq).await {
            if let Some(c) = cands.iter().find(|c| c.format == "epub").or_else(|| cands.first()) {
                best = Some(c.clone());
                break;
            }
        }
    }
    let c = best.ok_or_else(|| anyhow::anyhow!("no candidate found for '{query}'"))?;
    crate::books::download_and_import(state, &c).await
}

// ---- handlers ----

pub async fn following(State(state): State<AppState>) -> Html<String> {
    let authors: Vec<String> = get_json(&state, FOLLOWS_KEY).await;
    let pending = sqlx::query(
        "SELECT payload, status FROM jobs WHERE kind='acquire' AND status IN ('queued','running','failed')
         ORDER BY id DESC LIMIT 50",
    )
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default();
    let body = html! {
        header .pagehead {
            span .ph-icon { (maud::PreEscaped(crate::icons::HEART)) }
            h1 { "Wanted" }
        }
        @if !pending.is_empty() {
            table .arr-table {
                thead { tr { th { "Title" } th { "Status" } } }
                tbody {
                    @for r in &pending {
                        @let payload: String = r.get("payload");
                        @let status: String = r.get("status");
                        @let title = serde_json::from_str::<serde_json::Value>(&payload).ok()
                            .and_then(|v| v.get("query").and_then(|q| q.as_str()).map(String::from))
                            .unwrap_or_else(|| "…".into());
                        tr {
                            td .t-title { (title) }
                            td {
                                span .status-badge
                                    .queued[status == "queued"]
                                    .downloading[status == "running"]
                                    .failed[status == "failed"] { (status) }
                            }
                        }
                    }
                }
            }
        }
        h2 { "Followed authors" }
        @if authors.is_empty() { p .muted { "Follow authors to track their books." } }
        div .results {
            @for a in &authors {
                div .result {
                    span .title { (a) }
                    a .btn .btn-sm href={ "/add-new?q=" (urlenc(a)) } {
                        span .icon { (maud::PreEscaped(crate::icons::ROBOT)) } "Search"
                    }
                    form .inline hx-post="/following/remove" hx-swap="none" {
                        input type="hidden" name="author" value=(a);
                        button .link { "unfollow" }
                    }
                }
            }
        }
        form .search action="/following/add" method="post" {
            input type="text" name="author" placeholder="Author name…" required;
            button .btn type="submit" { "Follow" }
        }
        h2 { "Manual import" }
        p .muted { "One title per line — each is searched and the best match is grabbed." }
        form action="/following/import" method="post" {
            textarea .ta name="titles" rows="6" placeholder="The Hobbit\nDune\n…" {}
            div style="margin-top:.6rem" { button .btn type="submit" { "Import all" } }
        }
    };
    Html(page("Wanted", body).into_string())
}

fn urlenc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[derive(Deserialize)]
pub struct AuthorForm {
    pub author: String,
}
#[derive(Deserialize)]
pub struct ImportForm {
    pub titles: String,
}

pub async fn follow_add(State(state): State<AppState>, Form(f): Form<AuthorForm>) -> Response {
    let mut authors: Vec<String> = get_json(&state, FOLLOWS_KEY).await;
    let a = f.author.trim().to_string();
    if !a.is_empty() && !authors.iter().any(|x| x.eq_ignore_ascii_case(&a)) {
        authors.push(a);
        let _ = set_json(&state, FOLLOWS_KEY, &authors).await;
    }
    Redirect::to("/following").into_response()
}

pub async fn follow_remove(State(state): State<AppState>, Form(f): Form<AuthorForm>) -> Response {
    let authors: Vec<String> = get_json::<Vec<String>>(&state, FOLLOWS_KEY)
        .await
        .into_iter()
        .filter(|a| a != &f.author)
        .collect();
    let _ = set_json(&state, FOLLOWS_KEY, &authors).await;
    (axum::http::StatusCode::OK, "removed").into_response()
}

pub async fn import_list(State(state): State<AppState>, Form(f): Form<ImportForm>) -> Response {
    let mut n = 0;
    for line in f.titles.lines() {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        let _ = crate::jobs::enqueue(&state.pool, "acquire", &serde_json::json!({ "query": t })).await;
        n += 1;
    }
    let body = html! {
        section .empty {
            h1 { "Queued " (n) " title" @if n != 1 { "s" } }
            p { "They'll appear in your " a href="/" { "library" } " as they download." }
        }
    };
    Html(page("Import", body).into_string()).into_response()
}
