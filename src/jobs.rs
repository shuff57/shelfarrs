//! In-process background queue: a `jobs` table + one tokio worker that claims,
//! runs, and finalizes jobs. No Redis.

use crate::AppState;
use anyhow::Result;
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

async fn run(state: &AppState, job: &Job) -> Result<()> {
    match job.kind.as_str() {
        "scan" => {
            let n = crate::books::scan_dir(state).await?;
            tracing::info!("scan imported {n} new books");
            Ok(())
        }
        "download" => {
            let cand: crate::source::Candidate = serde_json::from_str(&job.payload)?;
            crate::books::download_and_import(state, &cand).await
        }
        other => anyhow::bail!("unknown job kind: {other}"),
    }
}
