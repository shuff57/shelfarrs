mod auth;
mod autosearch;
mod books;
mod discovery;
mod downloads;
mod indexer;
mod install;
mod jobs;
mod meta;
mod opds;
mod plugin;
mod reader;
mod score;
mod source;

use axum::{
    extract::{Query, State},
    response::Html,
    routing::{get, post},
    Extension, Router,
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
    pub covers_dir: PathBuf,
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

// Inline line-icons (stroke = currentColor), Listenarr/Phosphor-style glyphs.
pub mod icons {
    pub const BOOKS: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M4 3h5v18H4z"/><path d="M9 3h5v18H9z"/><path d="M15.5 4.2l4.8 1.3-4.6 16.3-4.8-1.3z"/></svg>"#;
    pub const PLUS: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><path d="M12 5v14M5 12h14"/></svg>"#;
    pub const CALENDAR: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="5" width="18" height="16" rx="2"/><path d="M8 3v4M16 3v4M3 10h18"/></svg>"#;
    pub const FOLDER_OPEN: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M3 7V5a2 2 0 0 1 2-2h4l2 3h8a2 2 0 0 1 2 2v2"/><path d="M3 7h18l-2.5 11a2 2 0 0 1-2 1.5H7.5a2 2 0 0 1-2-1.5z"/></svg>"#;
    pub const ACTIVITY: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M2 12h4l3-8 6 16 3-8h4"/></svg>"#;
    pub const HEART: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M20.8 4.6a5.5 5.5 0 0 0-7.8 0L12 5.6l-1-1a5.5 5.5 0 0 0-7.8 7.8l1 1L12 21.2l7.8-7.8 1-1a5.5 5.5 0 0 0 0-7.8z"/></svg>"#;
    pub const GEAR: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="3.2"/><path d="M19 12a7 7 0 0 0-.14-1.4l2-1.55-2-3.46-2.36.95a7 7 0 0 0-2.42-1.4L13.7 2.6h-3.4l-.38 2.54a7 7 0 0 0-2.42 1.4l-2.36-.95-2 3.46 2 1.55a7.06 7.06 0 0 0 0 2.8l-2 1.55 2 3.46 2.36-.95a7 7 0 0 0 2.42 1.4l.38 2.54h3.4l.38-2.54a7 7 0 0 0 2.42-1.4l2.36.95 2-3.46-2-1.55A7 7 0 0 0 19 12z"/></svg>"#;
    pub const MONITOR: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="2" y="4" width="20" height="13" rx="2"/><path d="M8 21h8M12 17v4"/></svg>"#;
    pub const SEARCH: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><circle cx="11" cy="11" r="7"/><path d="M21 21l-4.5-4.5"/></svg>"#;
    pub const BELL: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M18 9a6 6 0 1 0-12 0c0 6-2.5 7-2.5 7h17S18 15 18 9z"/><path d="M10 20a2.2 2.2 0 0 0 4 0"/></svg>"#;
    pub const USER: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="8" r="4"/><path d="M4 21c0-4 3.6-6 8-6s8 2 8 6"/></svg>"#;
    pub const GRID: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linejoin="round"><rect x="4" y="4" width="7" height="7" rx="1"/><rect x="13" y="4" width="7" height="7" rx="1"/><rect x="4" y="13" width="7" height="7" rx="1"/><rect x="13" y="13" width="7" height="7" rx="1"/></svg>"#;
    pub const LIST: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><path d="M9 6h11M9 12h11M9 18h11M4 6h.01M4 12h.01M4 18h.01"/></svg>"#;
    pub const INFO: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><circle cx="12" cy="12" r="9"/><path d="M12 11v6M12 7.5v.01"/></svg>"#;
    pub const CARET_DOWN: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M6 9l6 6 6-6"/></svg>"#;
    pub const REFRESH: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M20 5v5h-5"/><path d="M4 19v-5h5"/><path d="M20 10a8 8 0 0 0-14.5-3.5M4 14a8 8 0 0 0 14.5 3.5"/></svg>"#;
    pub const X: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><path d="M6 6l12 12M18 6L6 18"/></svg>"#;
    pub const BOOK_OPEN: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M2 4h6a4 4 0 0 1 4 4v12a3 3 0 0 0-3-3H2z"/><path d="M22 4h-6a4 4 0 0 0-4 4v12a3 3 0 0 1 3-3h7z"/></svg>"#;
    pub const ROBOT: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><rect x="4" y="8" width="16" height="12" rx="2"/><path d="M12 4v4M8 4h8"/><path d="M9 13.5h.01M15 13.5h.01"/><path d="M9 17h6"/></svg>"#;
    pub const CODE: &str = r#"<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M8 6l-6 6 6 6M16 6l6 6-6 6"/></svg>"#;
}
pub const LOGO: &str = r##"<svg viewBox="0 0 24 24" fill="none" stroke="#2196f3" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M2 4h6a4 4 0 0 1 4 4v12a3 3 0 0 0-3-3H2z"/><path d="M22 4h-6a4 4 0 0 0-4 4v12a3 3 0 0 1 3-3h7z"/></svg>"##;

fn nav_item(href: &str, label: &str, icon: &str, active: bool, badge: Option<&str>) -> Markup {
    html! {
        a .item .active[active] href=(href) {
            span .icon { (maud::PreEscaped(icon)) }
            span .label { (label) }
            @if let Some(id) = badge {
                span .pill-slot hx-get={ "/nav/badge/" (id) } hx-trigger="load" hx-swap="outerHTML" {}
            }
        }
    }
}

fn sub_item(href: &str, label: &str, active: bool) -> Markup {
    html! { a .subitem .active[active] href=(href) { (label) } }
}

/// Wrap page chrome (topbar + sidebar + htmx) around body content. The active
/// nav item and sub-menus are inferred from the page title.
pub fn page(title: &str, body: Markup) -> Markup {
    let lib = matches!(title, "Library" | "Authors" | "Series" | "Collection");
    let settings = matches!(
        title,
        "Settings"
            | "Plugins"
            | "Indexers"
            | "Download Clients"
            | "Quality Profiles"
            | "Users"
            | "Account"
            | "General"
    );
    let nav = html! {
        div .section {
            (nav_item("/?group=books", "Books", icons::BOOKS, lib, None))
            @if lib {
                div .subnav {
                    (sub_item("/?group=books", "Books", title == "Library"))
                    (sub_item("/?group=authors", "Authors", title == "Authors"))
                    (sub_item("/?group=series", "Series", title == "Series"))
                }
            }
            (nav_item("/add-new", "Add New", icons::PLUS, title == "Add New", None))
            (nav_item("/calendar", "Calendar", icons::CALENDAR, title == "Calendar", None))
            (nav_item("/library-import", "Library Import", icons::FOLDER_OPEN, title == "Library Import", None))
        }
        div .section {
            (nav_item("/activity", "Activity", icons::ACTIVITY, title == "Activity", Some("activity")))
            (nav_item("/wanted", "Wanted", icons::HEART, title == "Wanted", Some("wanted")))
        }
        div .section {
            (nav_item("/settings", "Settings", icons::GEAR, settings, None))
            @if settings {
                div .subnav {
                    (sub_item("/settings?tab=plugins", "Plugins", title == "Plugins" || title == "Settings"))
                    (sub_item("/settings?tab=indexers", "Indexers", title == "Indexers"))
                    (sub_item("/settings?tab=clients", "Download Clients", title == "Download Clients"))
                    (sub_item("/settings?tab=profiles", "Quality Profiles", title == "Quality Profiles"))
                    (sub_item("/settings?tab=users", "Users", title == "Users" || title == "Account"))
                    (sub_item("/settings?tab=general", "General", title == "General"))
                }
            }
            (nav_item("/system", "System", icons::MONITOR, title == "System", None))
        }
    };
    shell(
        title,
        html! {
            header .topbar {
                a href="/" .brand {
                    span .logo { (maud::PreEscaped(LOGO)) }
                    h1 { "Shelfarrs" }
                }
                div .nav-actions {
                    div .topsearch {
                        span .icon { (maud::PreEscaped(icons::SEARCH)) }
                        input type="search" name="q" placeholder="Search your library..."
                            hx-get="/search/suggest" hx-trigger="input changed delay:250ms, focus"
                            hx-target="#suggest" autocomplete="off";
                        div #suggest .suggest {}
                    }
                    details .menu {
                        summary .iconbtn { (maud::PreEscaped(icons::BELL)) }
                        div .dropdown hx-get="/notif/recent" hx-trigger="load" { p .muted { "…" } }
                    }
                    details .menu {
                        summary .iconbtn { (maud::PreEscaped(icons::USER)) }
                        div .dropdown { a href="/logout" { "Logout" } }
                    }
                }
            }
            aside .sidebar {
                nav { (nav) }
                div .sidefoot {
                    span { "v" (env!("CARGO_PKG_VERSION")) }
                    a href="https://github.com/shuff57/shelfarrs" target="_blank" .srclink { (maud::PreEscaped(icons::CODE)) }
                }
            }
            main { (body) }
        },
    )
}

/// Bare chrome (no sidebar/topbar) — the login screen.
pub fn page_bare(title: &str, body: Markup) -> Markup {
    shell(title, html! { main .bare { (body) } })
}

// ---- Settings (horizontal tabs, Listenarr-style) ----

#[derive(serde::Deserialize)]
pub struct TabQ {
    tab: Option<String>,
    preset: Option<String>,
}

async fn settings_page(
    State(state): State<AppState>,
    Extension(me): Extension<auth::CurrentUser>,
    Query(q): Query<TabQ>,
) -> Html<String> {
    let tab = q.tab.as_deref().unwrap_or("plugins");
    let known = ["plugins", "indexers", "clients", "profiles", "users", "general"];
    let tab = if known.contains(&tab) { tab } else { "plugins" };
    let (title, content) = match tab {
        "indexers" => ("Indexers", indexer::indexers_body(&state, q.preset.as_deref()).await),
        "clients" => ("Download Clients", downloads::clients_body(&state).await),
        "profiles" => ("Quality Profiles", score::profiles_body(&state).await),
        "users" => ("Users", auth::users_body(&state, &me).await),
        "general" => ("General", general_body(&state)),
        _ => ("Plugins", install::plugins_body(&state).await),
    };
    let tab_link = |t: &str, icon: &str, label: &str| {
        html! {
            a .tab .active[t == tab] href={ "/settings?tab=" (t) } {
                span .icon { (maud::PreEscaped(icon)) }
                (label)
            }
        }
    };
    let body = html! {
        header .pagehead { span .ph-icon { (maud::PreEscaped(icons::GEAR)) } h1 { "Settings" } }
        div .tabs {
            (tab_link("plugins", icons::BOOKS, "Plugins"))
            (tab_link("indexers", icons::SEARCH, "Indexers"))
            (tab_link("clients", icons::FOLDER_OPEN, "Download Clients"))
            (tab_link("profiles", icons::HEART, "Quality Profiles"))
            (tab_link("users", icons::USER, "Users"))
            (tab_link("general", icons::GEAR, "General"))
        }
        div .tab-panel { (content) }
    };
    Html(page(title, body).into_string())
}

fn general_body(state: &AppState) -> Markup {
    html! {
        h2 { "Paths" }
        div .cards {
            div .statcard { span .k { "Books folder" } span .v { code { (state.books_dir.display()) } } }
            div .statcard { span .k { "Plugins folder" } span .v { code { (state.plugins_dir.display()) } } }
            div .statcard { span .k { "Covers folder" } span .v { code { (state.covers_dir.display()) } } }
        }
        h2 { "Access" }
        div .cards {
            div .statcard { span .k { "OPDS catalog" } span .v { code { "/opds" } " (HTTP Basic)" } }
            div .statcard { span .k { "Version" } span .v { "v" (env!("CARGO_PKG_VERSION")) } }
        }
    }
}

// ---- System (status cards, Listenarr-style) ----

async fn system_page(State(state): State<AppState>) -> Html<String> {
    let count = |sql: &str| {
        let pool = state.pool.clone();
        let sql = sql.to_string();
        async move {
            sqlx::query_scalar::<_, i64>(&sql).fetch_one(&pool).await.unwrap_or(0)
        }
    };
    let books = count("SELECT COUNT(*) FROM books").await;
    let failed = count("SELECT COUNT(*) FROM jobs WHERE status='failed'").await;
    let done = count("SELECT COUNT(*) FROM jobs WHERE status='done'").await;
    let users = count("SELECT COUNT(*) FROM users").await;
    let plugins = plugin::scan_installed(&state.plugins_dir).len();
    let db_kb = std::env::var("DATA_DIR")
        .ok()
        .and_then(|d| std::fs::metadata(format!("{d}/shelfarr.db")).ok())
        .or_else(|| std::fs::metadata("data/shelfarr.db").ok())
        .map(|m| m.len() / 1024)
        .unwrap_or(0);
    let body = html! {
        header .pagehead { span .ph-icon { (maud::PreEscaped(icons::MONITOR)) } h1 { "System" } }
        h2 { "Status" }
        div .cards {
            div .statcard { span .k { "Version" } span .v { "v" (env!("CARGO_PKG_VERSION")) } }
            div .statcard { span .k { "Books" } span .v { (books) } }
            div .statcard { span .k { "Plugins installed" } span .v { (plugins) } }
            div .statcard { span .k { "Users" } span .v { (users) } }
            div .statcard { span .k { "Database" } span .v { (db_kb) " KB" } }
            div .statcard .warn[failed > 0] { span .k { "Jobs" } span .v { (done) " done · " (failed) " failed" } }
        }
    };
    Html(page("System", body).into_string())
}

fn shell(title: &str, content: Markup) -> Markup {
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
            body { (content) }
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
    let covers_dir = PathBuf::from(format!("{data_dir}/covers"));
    std::fs::create_dir_all(&covers_dir)?;

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
        covers_dir,
        plugins_dir,
        http,
    };

    // Kick off an initial library scan and start the workers.
    jobs::enqueue(&state.pool, "scan", &serde_json::json!({})).await?;
    tokio::spawn(jobs::worker(state.clone()));
    tokio::spawn(downloads::monitor(state.clone()));
    tokio::spawn(autosearch::worker(state.clone()));

    let app = Router::new()
        .route("/", get(books::library))
        .route("/collection/{kind}/{name}", get(books::collection))
        .route("/add-new", get(books::discover))
        .route("/discover", get(|| async { axum::response::Redirect::permanent("/add-new") }))
        .route("/calendar", get(books::calendar))
        .route("/library-import", get(books::library_import))
        .route("/library-import/scan", post(books::scan_now))
        .route("/activity", get(jobs::activity_page))
        .route("/add", post(books::add))
        .route("/grab", post(indexer::grab))
        .route("/books/{id}", get(books::book_detail))
        .route("/books/{id}/edit", post(books::book_edit))
        .route("/books/{id}/delete", post(books::book_delete))
        .route("/books/{id}/file", get(books::book_file))
        .route("/books/{id}/cover", get(books::book_cover))
        .route("/read/{id}", get(reader::read))
        .route("/progress/{id}", post(reader::save_progress))
        .route("/opds", get(opds::feed))
        .route("/nav/badge/{id}", get(jobs::nav_badge))
        .route("/search/suggest", get(books::search_suggest))
        .route("/notif/recent", get(jobs::notif_recent))
        .route("/system", get(system_page))
        .route("/wanted", get(discovery::following))
        .route("/following", get(|| async { axum::response::Redirect::permanent("/wanted") }))
        .route("/following/add", post(discovery::follow_add))
        .route("/following/remove", post(discovery::follow_remove))
        .route("/following/import", post(discovery::import_list))
        .route("/settings", get(settings_page))
        .route("/books/{id}/auto-search", post(autosearch::search_now))
        .route("/settings/profiles/add", post(score::add))
        .route("/settings/profiles/delete", post(score::delete))
        .route("/settings/profiles/default", post(score::make_default))
        .route("/settings/clients/add", post(downloads::add))
        .route("/settings/clients/delete", post(downloads::delete))
        .route("/settings/clients/test", post(downloads::test_handler))
        .route("/settings/clients/mappings/add", post(downloads::mapping_add))
        .route("/settings/clients/mappings/delete", post(downloads::mapping_delete))
        .route("/settings/indexers/add", post(indexer::add))
        .route("/settings/indexers/delete", post(indexer::delete))
        .route("/settings/indexers/toggle", post(indexer::toggle))
        .route("/settings/indexers/test", post(indexer::test_handler))
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
