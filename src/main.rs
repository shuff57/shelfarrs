mod books;
mod jobs;
mod source;

use axum::{
    routing::{get, post},
    Router,
};
use maud::{html, Markup, DOCTYPE};
use source::{Gutenberg, Source};
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tower_http::services::ServeDir;

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    pub sources: Arc<HashMap<String, Box<dyn Source>>>,
    pub books_dir: PathBuf,
    pub http: reqwest::Client,
}

/// Wrap page chrome (nav + htmx) around body content.
pub fn page(title: &str, body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " · Shelfarr" }
                link rel="stylesheet" href="/assets/style.css";
                script src="/assets/htmx.min.js" {}
            }
            body {
                header .topbar {
                    a href="/" .brand { "Shelfarr" }
                    nav { a href="/" { "Library" } a href="/discover" { "Discover" } }
                }
                main { (body) }
            }
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "data".into());
    std::fs::create_dir_all(&data_dir)?;
    let db_url = format!("sqlite:{data_dir}/shelfarr.db?mode=rwc");
    let pool: SqlitePool = SqlitePoolOptions::new().connect(&db_url).await?;
    sqlx::migrate!().run(&pool).await?;

    let books_dir = PathBuf::from(std::env::var("BOOKS_DIR").unwrap_or_else(|_| "books".into()));
    std::fs::create_dir_all(&books_dir)?;

    let http = reqwest::Client::builder()
        .user_agent("shelfarr-rs/0.0")
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(300))
        .build()?;

    let mut sources: HashMap<String, Box<dyn Source>> = HashMap::new();
    sources.insert("gutenberg".into(), Box::new(Gutenberg::new(http.clone())));

    let state = AppState {
        pool,
        sources: Arc::new(sources),
        books_dir,
        http,
    };

    // Kick off an initial library scan and start the worker.
    jobs::enqueue(&state.pool, "scan", &serde_json::json!({})).await?;
    tokio::spawn(jobs::worker(state.clone()));

    let app = Router::new()
        .route("/", get(books::library))
        .route("/discover", get(books::discover))
        .route("/add", post(books::add))
        .route("/books/{id}", get(books::book_detail))
        .route("/books/{id}/file", get(books::book_file))
        .route("/healthz", get(|| async { "ok" }))
        .nest_service("/assets", ServeDir::new("assets"))
        .with_state(state);

    let addr = std::env::var("BIND").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("shelfarr listening on {addr}");
    axum::serve(listener, app).await?;
    Ok(())
}
