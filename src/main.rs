use axum::{response::Html, routing::get, Router};
use maud::{html, Markup, DOCTYPE};
use sqlx::sqlite::SqlitePoolOptions;
use sqlx::SqlitePool;
use tower_http::services::ServeDir;

/// Wrap page chrome around body content.
fn page(title: &str, body: Markup) -> Markup {
    html! {
        (DOCTYPE)
        html lang="en" {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " · Shelfarr" }
                link rel="stylesheet" href="/assets/style.css";
            }
            body {
                header .topbar { a href="/" .brand { "Shelfarr" } }
                main { (body) }
            }
        }
    }
}

async fn library() -> Html<String> {
    let body = html! {
        section .empty {
            h1 { "Your library is empty" }
            p { "Imported books will appear here." }
        }
    };
    Html(page("Library", body).into_string())
}

async fn healthz() -> &'static str {
    "ok"
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    // Config volume lives here in Docker (DATA_DIR=/data); ./data locally.
    let data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "data".into());
    std::fs::create_dir_all(&data_dir)?;
    let db_url = format!("sqlite:{data_dir}/shelfarr.db?mode=rwc");

    let pool: SqlitePool = SqlitePoolOptions::new().connect(&db_url).await?;
    sqlx::migrate!().run(&pool).await?;

    let app = Router::new()
        .route("/", get(library))
        .route("/healthz", get(healthz))
        .nest_service("/assets", ServeDir::new("assets"))
        .with_state(pool);

    let addr = std::env::var("BIND").unwrap_or_else(|_| "0.0.0.0:8080".into());
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("shelfarr listening on {addr}");
    axum::serve(listener, app).await?;
    Ok(())
}
