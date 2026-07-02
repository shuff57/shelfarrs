mod auth;
mod books;
mod discovery;
mod install;
mod jobs;
mod opds;
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
                title { (title) " · Shelfarrs" }
                link rel="stylesheet" href="/assets/style.css";
                script src="/assets/htmx.min.js" {}
            }
            body {
                header .topbar {
                    a href="/" .brand { "Shelfarrs" }
                    nav {
                    a href="/" { "Library" }
                    a href="/discover" { "Discover" }
                    a href="/following" { "Following" }
                    a href="/settings/plugins" { "Plugins" }
                    a href="/settings/users" { "Account" }
                }
                }
                main { (body) }
            }
        }
    }
}

/// Copy bundled default plugins into the runtime (volume) dir, skipping any that
/// already exist — so a redeploy adds new defaults without touching installed ones.
fn seed_plugins(bundled: &std::path::Path, runtime: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(bundled) else {
        return;
    };
    for e in entries.flatten() {
        let dest = runtime.join(e.file_name());
        if e.path().is_dir() && !dest.exists() {
            if let Err(err) = copy_dir(&e.path(), &dest) {
                tracing::warn!("could not seed plugin {}: {err}", e.file_name().to_string_lossy());
            } else {
                tracing::info!("seeded default plugin: {}", e.file_name().to_string_lossy());
            }
        }
    }
}

fn copy_dir(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for e in std::fs::read_dir(src)?.flatten() {
        let (from, to) = (e.path(), dst.join(e.file_name()));
        if from.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    // All mutable state lives under DATA_DIR so a single persistent volume mount
    // preserves the DB (progress/users/config), downloaded books, and installed
    // plugins across image rebuilds/redeploys.
    let data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "data".into());
    std::fs::create_dir_all(&data_dir)?;
    let db_url = format!("sqlite:{data_dir}/shelfarr.db?mode=rwc");
    let pool: SqlitePool = SqlitePoolOptions::new().connect(&db_url).await?;
    sqlx::migrate!().run(&pool).await?;
    auth::bootstrap_admin(&pool).await?;

    let books_dir =
        PathBuf::from(std::env::var("BOOKS_DIR").unwrap_or_else(|_| format!("{data_dir}/books")));
    std::fs::create_dir_all(&books_dir)?;

    let http = reqwest::Client::builder()
        .user_agent("shelfarrs/0.0")
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(300))
        .build()?;

    // Plugins live on the volume too. Seed the read-only bundled defaults
    // (SEED_PLUGINS_DIR, baked into the image) per-plugin-if-missing: installed
    // third-party plugins are never touched, and updates add new defaults.
    // ponytail: a removed default re-seeds on restart — drop it from the image to remove permanently.
    let plugins_dir =
        PathBuf::from(std::env::var("PLUGINS_DIR").unwrap_or_else(|_| format!("{data_dir}/plugins")));
    std::fs::create_dir_all(&plugins_dir)?;
    if let Ok(seed) = std::env::var("SEED_PLUGINS_DIR") {
        seed_plugins(std::path::Path::new(&seed), &plugins_dir);
    }
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
        .route("/opds", get(opds::feed))
        .route("/following", get(discovery::following))
        .route("/following/add", post(discovery::follow_add))
        .route("/following/remove", post(discovery::follow_remove))
        .route("/following/import", post(discovery::import_list))
        .route("/settings/plugins", get(install::plugins_page))
        .route("/settings/plugins/install", post(install::install))
        .route("/settings/plugins/uninstall", post(install::uninstall_handler))
        .route("/settings/plugins/repos/add", post(install::repos_add))
        .route("/settings/plugins/repos/remove", post(install::repos_remove))
        .route("/settings/users", get(auth::users_page))
        .route("/settings/users/add", post(auth::user_add))
        .route("/settings/users/remove", post(auth::user_remove))
        .route("/login", get(auth::login_page).post(auth::login))
        .route("/logout", get(auth::logout))
        .route("/healthz", get(|| async { "ok" }))
        .nest_service("/assets", ServeDir::new("assets"))
        .nest_service("/plugins", ServeDir::new(state.plugins_dir.clone()))
        .layer({
            let mw = state.clone();
            axum::middleware::from_fn(move |req, next| {
                let st = mw.clone();
                async move { auth::gate(st, req, next).await }
            })
        })
        .with_state(state);

    let addr = std::env::var("BIND").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("shelfarr listening on {addr}");
    axum::serve(listener, app).await?;
    Ok(())
}
