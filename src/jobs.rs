//! In-process background queue: a `jobs` table + one tokio worker that claims,
//! runs, and finalizes jobs. No Redis.

use crate::{page, AppState};
use anyhow::Result;
use axum::extract::{Path, Query, State};
use axum::response::Html;
use maud::{html, Markup};
use serde::Deserialize;
use sqlx::{Row, SqlitePool};
use std::time::Duration;

pub struct Job {
    pub id: i64,
    pub kind: String,
    pub payload: String,
}

pub async fn enqueue(pool: &SqlitePool, kind: &str, payload: &serde_json::Value) -> Result<i64> {
    let row = sqlx::query("INSERT INTO jobs (kind, payload) VALUES (?, ?) RETURNING id")
        .bind(kind)
        .bind(payload.to_string())
        .fetch_one(pool)
        .await?;
    Ok(row.get::<i64, _>("id"))
}

async fn claim(pool: &SqlitePool) -> Result<Option<Job>> {
    let row = sqlx::query(
        "UPDATE jobs SET status='running', updated_at=datetime('now')
         WHERE id = (SELECT id FROM jobs WHERE status='queued' ORDER BY id LIMIT 1)
         RETURNING id, kind, payload",
    )
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|r| Job {
        id: r.get("id"),
        kind: r.get("kind"),
        payload: r.get("payload"),
    }))
}

async fn finish(pool: &SqlitePool, id: i64, res: &Result<()>) {
    let (status, err) = match res {
        Ok(_) => ("done", None),
        Err(e) => ("failed", Some(e.to_string())),
    };
    if let Err(e) =
        sqlx::query("UPDATE jobs SET status=?, error=?, updated_at=datetime('now') WHERE id=?")
            .bind(status)
            .bind(err)
            .bind(id)
            .execute(pool)
            .await
    {
        tracing::error!("could not finalize job {id}: {e}");
    }
}

// ponytail: single worker + 2s poll. Add workers or a notify channel if throughput matters.
pub async fn worker(state: AppState) {
    loop {
        match claim(&state.pool).await {
            Ok(Some(job)) => {
                tracing::info!("job {} start ({})", job.id, job.kind);
                let res = run(&state, &job).await;
                finish(&state.pool, job.id, &res).await;
                match res {
                    Ok(_) => tracing::info!("job {} done", job.id),
                    Err(e) => tracing::error!("job {} failed: {e}", job.id),
                }
            }
            Ok(None) => tokio::time::sleep(Duration::from_secs(2)).await,
            Err(e) => {
                tracing::error!("claim failed: {e}");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}

// ---- Activity UI (jobs table, arr-style) ----

/// Human title for a job row, from its payload.
fn job_title(kind: &str, payload: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(payload).unwrap_or_default();
    match kind {
        "download" => v.get("title").and_then(|t| t.as_str()).unwrap_or("Download").to_string(),
        "acquire" => v
            .get("query")
            .and_then(|q| q.as_str())
            .map(|q| format!("Acquire: {q}"))
            .unwrap_or_else(|| "Acquire".into()),
        "scan" => "Library scan".into(),
        other => other.to_string(),
    }
}

/// (chip class, label, progress %) per job status — colors match the arr legend.
fn job_status(kind: &str, status: &str) -> (&'static str, &'static str, u8) {
    match status {
        "queued" => ("queued", "Queued", 0),
        "running" if kind == "scan" => ("processing", "Processing", 50),
        "running" => ("downloading", "Downloading", 50),
        "done" => ("completed", "Completed", 100),
        _ => ("failed", "Failed", 100),
    }
}

#[derive(Deserialize)]
pub struct ActivityQ {
    pub f: Option<String>,
}

pub async fn activity_page(State(state): State<AppState>, Query(q): Query<ActivityQ>) -> Html<String> {
    let rows = sqlx::query(
        "SELECT id, kind, payload, status, error, updated_at FROM jobs ORDER BY id DESC LIMIT 200",
    )
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default();
    let dls = sqlx::query(
        "SELECT title, protocol, state, progress, error, updated_at FROM downloads ORDER BY id DESC LIMIT 100",
    )
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default();
    let filter = q.f.unwrap_or_default().to_lowercase();

    let body = html! {
        header .pagehead {
            span .ph-icon { (maud::PreEscaped(crate::icons::ACTIVITY)) }
            h1 { "Activity" }
            form .headfilter action="/activity" method="get" {
                input type="search" name="f" value=(filter) placeholder="Filter activity...";
            }
            a .toolbar-btn href="/activity" { span .icon { (maud::PreEscaped(crate::icons::REFRESH)) } "Refresh" }
        }
        @if !dls.is_empty() {
            h2 { "Downloads" }
            table .arr-table {
                thead { tr { th { "Title" } th { "Protocol" } th { "Progress" } th { "Updated" } th { "Status" } } }
                tbody {
                    @for d in &dls {
                        @let title: String = d.get("title");
                        @let st: String = d.get("state");
                        @let progress: f64 = d.get("progress");
                        @let error: Option<String> = d.get("error");
                        @let class = match st.as_str() {
                            "queued" => "queued",
                            "downloading" => "downloading",
                            "completed" | "importing" => "processing",
                            "imported" => "completed",
                            _ => "failed",
                        };
                        @if filter.is_empty() || title.to_lowercase().contains(&filter) {
                            tr {
                                td .t-title { (title) }
                                td .t-muted { (d.get::<String, _>("protocol")) }
                                td { div .progress { div .bar .{ "p-" (class) } style={ "width:" ((progress * 100.0) as i64) "%" } {} } }
                                td .t-muted { (d.get::<String, _>("updated_at")) }
                                td { span .status-badge .{ (class) } title=[error] { (st) } }
                            }
                        }
                    }
                }
            }
            h2 { "Jobs" }
        }
        @if rows.is_empty() {
            div .empty-state {
                span .eicon { (maud::PreEscaped(crate::icons::ACTIVITY)) }
                p .empty-title { "No activity" }
                p .empty-message { "Downloads, imports and scans will show up here." }
            }
        } @else {
            table .arr-table {
                thead { tr { th { "Title" } th { "Type" } th { "Progress" } th { "Updated" } th { "Status" } } }
                tbody {
                    @for r in &rows {
                        @let kind: String = r.get("kind");
                        @let payload: String = r.get("payload");
                        @let status: String = r.get("status");
                        @let error: Option<String> = r.get("error");
                        @let title = job_title(&kind, &payload);
                        @let (class, label, pct) = job_status(&kind, &status);
                        @if filter.is_empty() || title.to_lowercase().contains(&filter) {
                            tr {
                                td .t-title { (title) }
                                td .t-muted { (kind) }
                                td { div .progress { div .bar .{ "p-" (class) } style={ "width:" (pct) "%" } {} } }
                                td .t-muted { (r.get::<String, _>("updated_at")) }
                                td {
                                    span .status-badge .{ (class) } title=[error] { (label) }
                                }
                            }
                        }
                    }
                }
            }
        }
    };
    Html(page("Activity", body).into_string())
}

/// Sidebar count pills, loaded via HTMX. Empty response = no pill.
pub async fn nav_badge(State(state): State<AppState>, Path(id): Path<String>) -> Html<String> {
    let n: i64 = match id.as_str() {
        "activity" => {
            let jobs: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM jobs WHERE status IN ('queued','running')")
                    .fetch_one(&state.pool)
                    .await
                    .unwrap_or(0);
            let dls: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM downloads WHERE state IN ('queued','downloading','completed','importing')",
            )
            .fetch_one(&state.pool)
            .await
            .unwrap_or(0);
            jobs + dls
        }
        "wanted" => sqlx::query_scalar(
            "SELECT COUNT(*) FROM jobs WHERE kind='acquire' AND status IN ('queued','running')",
        )
        .fetch_one(&state.pool)
        .await
        .unwrap_or(0),
        _ => 0,
    };
    if n > 0 {
        Html(html! { span .pill { (n) } }.into_string())
    } else {
        Html(String::new())
    }
}

/// Topbar bell dropdown: the latest finished/failed jobs.
pub async fn notif_recent(State(state): State<AppState>) -> Html<String> {
    let rows = sqlx::query(
        "SELECT kind, payload, status FROM jobs WHERE status IN ('done','failed') ORDER BY id DESC LIMIT 8",
    )
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default();
    let m: Markup = html! {
        p .drophead { "Recent Activity" }
        @if rows.is_empty() { p .muted { "Nothing yet." } }
        @for r in &rows {
            @let kind: String = r.get("kind");
            @let status: String = r.get("status");
            div .notif {
                span .dot .ok[status == "done"] .bad[status != "done"] {}
                span { (job_title(&kind, &r.get::<String, _>("payload"))) }
            }
        }
    };
    Html(m.into_string())
}

async fn run(state: &AppState, job: &Job) -> Result<()> {
    match job.kind.as_str() {
        "scan" => {
            let n = crate::books::scan_dir(state).await?;
            let e = crate::meta::enrich_pending(state).await;
            tracing::info!("scan imported {n} new books, enriched {e}");
            Ok(())
        }
        "download" => {
            let cand: crate::source::Candidate = serde_json::from_str(&job.payload)?;
            crate::books::download_and_import(state, &cand).await?;
            crate::meta::enrich_pending(state).await;
            Ok(())
        }
        "acquire" => {
            let v: serde_json::Value = serde_json::from_str(&job.payload)?;
            let query = v.get("query").and_then(|q| q.as_str()).unwrap_or_default();
            crate::discovery::acquire(state, query).await
        }
        other => anyhow::bail!("unknown job kind: {other}"),
    }
}
