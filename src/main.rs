mod books;
mod install;
mod jobs;
mod plugin;
mod reader;
mod source;

use axum::{
    routing::{get, post},
    Router,
};
use maud::{html, Markup, DOCTYPE};
use plugin::{KvStore, SourceMap};
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::SqlitePool;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use tower_http::services::ServeDir;

#[derive(Clone)]
pub struct AppState {
    pub pool: SqlitePool,
    /// Hot-swappable source registry: clone the inner Arc under the lock, then use
    /// it across awaits without holding the guard. Install/uninstall swaps it live.
    pub sources: Arc<RwLock<Arc<SourceMap>>>,
    pub kv: KvStore,
    pub books_dir: PathBuf,
    pub plugins_dir: PathBuf,
    pub http: reqwest::Client,
}

impl AppState {
    /// Snapshot the current source registry (cheap Arc clone).
    pub fn sources(&self) -> Arc<SourceMap> {
        self.sources.read().unwrap().clone()
    }
    /// Rebuild the registry from disk after an install/uninstall — no restart.
    pub fn reload_sources(&self) {
        let map = plugin::load_sources(&self.plugins_dir, self.kv.clone());
        *self.sources.write().unwrap() = Arc::new(map);
    }
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
                    nav {
                    a href="/" { "Library" }
                    a href="/discover" { "Discover" }
                    a href="/settings/plugins" { "Plugins" }
                }
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

    // Load source plugins (WASM, sandboxed) from the plugins dir.
    let plugins_dir = PathBuf::from(std::env::var("PLUGINS_DIR").unwrap_or_else(|_| "plugins".into()));
    std::fs::create_dir_all(&plugins_dir)?;
    let kv: KvStore = Arc::new(Mutex::new(HashMap::new()));
    let sources = plugin::load_sources(&plugins_dir, kv.clone());

    let state = AppState {
        pool,
        sources: Arc::new(RwLock::new(Arc::new(sources))),
        kv,
        books_dir,
        plugins_dir,
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
        .route("/read/{id}", get(reader::read))
        .route("/progress/{id}", post(reader::save_progress))
        .route("/settings/plugins", get(install::plugins_page))
        .route("/settings/plugins/install", post(install::install))
        .route("/settings/plugins/uninstall", post(install::uninstall_handler))
        .route("/settings/plugins/repos/add", post(install::repos_add))
        .route("/settings/plugins/repos/remove", post(install::repos_remove))
        .route("/healthz", get(|| async { "ok" }))
        .nest_service("/assets", ServeDir::new("assets"))
        .nest_service("/plugins", ServeDir::new(state.plugins_dir.clone()))
        .with_state(state);

    let addr = std::env::var("BIND").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("shelfarr listening on {addr}");
    axum::serve(listener, app).await?;
    Ok(())
}
