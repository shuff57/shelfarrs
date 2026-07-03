//! B4: automatic search. Every 6 hours, monitored books whose on-disk format
//! is below their profile's cutoff get an indexer search; the top-scored
//! release is grabbed as an upgrade. Also exposes the per-book manual trigger.

use crate::auth::{deny_non_admin, CurrentUser};
use crate::score::{self, format_rank};
use crate::AppState;
use anyhow::{Context, Result};
use axum::{
    extract::{Path, State},
    response::{Html, IntoResponse, Response},
    Extension,
};
use sqlx::Row;

pub struct WantedBook {
    pub id: i64,
    pub title: String,
    pub author: Option<String>,
    pub format: String,
    pub profile_id: Option<i64>,
}

/// Monitored books whose format ranks below their profile's cutoff.
pub async fn below_cutoff(state: &AppState) -> Vec<WantedBook> {
    let rows = sqlx::query(
        "SELECT id, title, author, format, quality_profile_id FROM books WHERE monitored=1",
    )
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default();
    let mut out = vec![];
    for r in rows {
        let b = WantedBook {
            id: r.get("id"),
            title: r.get("title"),
            author: r.get("author"),
            format: r.get("format"),
            profile_id: r.get("quality_profile_id"),
        };
        let p = score::profile_for(state, b.profile_id).await;
        if format_rank(&b.format) < format_rank(&p.cutoff) {
            out.push(b);
        }
    }
    out
}

/// Search + score + grab the best upgrade for one book. Returns what happened.
pub async fn search_one(state: &AppState, book: &WantedBook) -> Result<String> {
    let profile = score::profile_for(state, book.profile_id).await;
    let query = match &book.author {
        Some(a) => format!("{} {}", book.title, a),
        None => book.title.clone(),
    };
    let releases = crate::indexer::search_all(state, &query).await;
    if releases.is_empty() {
        return Ok(format!("no releases found for '{}'", book.title));
    }
    // Only grab an actual upgrade over what's on disk.
    let current = format_rank(&book.format);
    let upgrades: Vec<crate::indexer::Release> = releases
        .into_iter()
        .filter(|r| {
            score::detect_format(&r.title).map_or(true, |f| format_rank(f) > current)
        })
        .collect();
    match score::pick_best(&upgrades, &profile) {
        Some((best, s)) => {
            crate::downloads::grab_release(state, best)
                .await
                .with_context(|| format!("grabbing '{}'", best.title))?;
            Ok(format!("grabbed '{}' (score {s})", best.title))
        }
        None => Ok(format!("no acceptable upgrade for '{}'", book.title)),
    }
}

/// The 6-hour background loop (5-minute startup delay, arr-style).
pub async fn worker(state: AppState) {
    tokio::time::sleep(std::time::Duration::from_secs(300)).await;
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(6 * 3600));
    loop {
        tick.tick().await;
        let wanted = below_cutoff(&state).await;
        if wanted.is_empty() {
            continue;
        }
        tracing::info!("auto-search: {} monitored book(s) below cutoff", wanted.len());
        for b in wanted {
            match search_one(&state, &b).await {
                Ok(msg) => tracing::info!("auto-search: {msg}"),
                Err(e) => tracing::warn!("auto-search '{}': {e}", b.title),
            }
        }
    }
}

/// Manual per-book trigger (Wanted page button).
pub async fn search_now(
    State(state): State<AppState>,
    Extension(me): Extension<CurrentUser>,
    Path(id): Path<i64>,
) -> Response {
    if let Some(r) = deny_non_admin(&me) {
        return r;
    }
    let row = sqlx::query(
        "SELECT id, title, author, format, quality_profile_id FROM books WHERE id=?",
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await
    .ok()
    .flatten();
    let Some(r) = row else {
        return Html("<span class=\"err\">no such book</span>".to_string()).into_response();
    };
    let b = WantedBook {
        id: r.get("id"),
        title: r.get("title"),
        author: r.get("author"),
        format: r.get("format"),
        profile_id: r.get("quality_profile_id"),
    };
    match search_one(&state, &b).await {
        Ok(msg) => Html(format!("<span class=\"queued\">{msg}</span>")).into_response(),
        Err(e) => Html(format!("<span class=\"err\">{e}</span>")).into_response(),
    }
}
