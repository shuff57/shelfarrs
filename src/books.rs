//! Library: model, filesystem scan, download→import, byte serving, and the
//! HTMX-rendered library / discover pages.

use crate::source::{Candidate, SearchQuery};
use crate::{page, AppState};
use anyhow::{Context, Result};
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{Request, StatusCode},
    response::{Html, IntoResponse, Response},
    Form,
};
use maud::html;
use serde::Deserialize;
use sqlx::Row;
use std::path::PathBuf;
use tower::ServiceExt;
use tower_http::services::ServeFile;

#[derive(sqlx::FromRow)]
pub struct Book {
    pub id: i64,
    pub title: String,
    pub author: Option<String>,
    pub format: String,
    pub size: Option<i64>,
    pub description: Option<String>,
    pub cover: Option<String>,
}

const BOOK_EXTS: &[&str] = &["epub", "pdf", "cbz", "cbr", "mobi", "azw3", "txt"];

/// Recognised book format from a path's extension, lowercased.
fn book_format(p: &std::path::Path) -> Option<String> {
    let ext = p.extension()?.to_str()?.to_lowercase();
    BOOK_EXTS.contains(&ext.as_str()).then_some(ext)
}

/// Strip anything that could escape the books dir or break a filename.
fn sanitize(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.').trim();
    if trimmed.is_empty() {
        "book".into()
    } else {
        trimmed.chars().take(180).collect()
    }
}

/// Recursively collect files under `dir`.
fn walk(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else {
            out.push(p);
        }
    }
}

/// Import any new book files found under BOOKS_DIR. Returns count of new rows.
pub async fn scan_dir(state: &AppState) -> Result<usize> {
    let mut files = vec![];
    walk(&state.books_dir, &mut files);
    let mut count = 0;
    for f in files {
        let Some(fmt) = book_format(&f) else { continue };
        let title = f
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "Untitled".into());
        let path = f.to_string_lossy().to_string();
        let size = std::fs::metadata(&f).ok().map(|m| m.len() as i64);
        let res = sqlx::query(
            "INSERT OR IGNORE INTO books (title, format, path, size) VALUES (?,?,?,?)",
        )
        .bind(&title)
        .bind(&fmt)
        .bind(&path)
        .bind(size)
        .execute(&state.pool)
        .await?;
        count += res.rows_affected() as usize;
    }
    Ok(count)
}

/// Fetch a candidate's bytes (host does the networking), write into BOOKS_DIR, index it.
pub async fn download_and_import(state: &AppState, cand: &Candidate) -> Result<()> {
    let sources = state.sources();
    let source = sources
        .get(&cand.source)
        .with_context(|| format!("unknown source: {}", cand.source))?
        .clone();
    let dl = source.resolve_download(&cand.reference).await?;

    let bytes = if let Some(b) = dl.bytes {
        b
    } else {
        let url = dl.url.context("download had neither bytes nor url")?;
        let mut req = state.http.get(&url);
        for (k, v) in &dl.headers {
            req = req.header(k, v);
        }
        req.send()
            .await?
            .error_for_status()?
            .bytes()
            .await?
            .to_vec()
    };

    std::fs::create_dir_all(&state.books_dir)?;
    let fname = format!("{}.{}", sanitize(&cand.title), cand.format);
    let path = state.books_dir.join(&fname);
    std::fs::write(&path, &bytes)?;

    sqlx::query("INSERT OR IGNORE INTO books (title, author, format, path, size) VALUES (?,?,?,?,?)")
        .bind(&cand.title)
        .bind(&cand.author)
        .bind(&cand.format)
        .bind(path.to_string_lossy().as_ref())
        .bind(bytes.len() as i64)
        .execute(&state.pool)
        .await?;
    Ok(())
}

async fn all_books(state: &AppState) -> Result<Vec<Book>> {
    Ok(
        sqlx::query_as::<_, Book>(
            "SELECT id,title,author,format,size,description,cover FROM books ORDER BY title",
        )
            .fetch_all(&state.pool)
            .await?,
    )
}

// ---- HTTP handlers ----

pub async fn library(State(state): State<AppState>) -> Html<String> {
    let books = all_books(&state).await.unwrap_or_default();
    let body = if books.is_empty() {
        html! {
            section .empty {
                h1 { "Your library is empty" }
                p { "Scan " code { "BOOKS_DIR" } " or " a href="/discover" { "discover books" } "." }
            }
        }
    } else {
        html! {
            header .pagehead {
                h1 { "Library" }
                span .chip .primary { (books.len()) @if books.len() == 1 { " Book" } @else { " Books" } }
            }
            section .grid {
                @for b in &books {
                    a .card href={ "/books/" (b.id) } {
                        div .poster {
                            @if b.cover.is_some() {
                                img .cover src={ "/books/" (b.id) "/cover" } alt="" loading="lazy";
                            } @else {
                                div .cover-ph { span { (b.format) } }
                            }
                            div .status-bar {}
                        }
                        span .title { (b.title) }
                        @if let Some(a) = &b.author { span .author { (a) } }
                    }
                }
            }
        }
    };
    Html(page("Library", body).into_string())
}

pub async fn book_detail(State(state): State<AppState>, Path(id): Path<i64>) -> Response {
    let book = sqlx::query_as::<_, Book>(
        "SELECT id,title,author,format,size,description,cover FROM books WHERE id=?",
    )
    .bind(id)
    .fetch_optional(&state.pool)
    .await
    .ok()
    .flatten();
    let Some(b) = book else {
        return (StatusCode::NOT_FOUND, "no such book").into_response();
    };
    let has_viewer = crate::plugin::viewer_for(&state.plugins_dir, &b.format).is_some();
    let body = html! {
        article .detail {
            @if b.cover.is_some() { img .cover src={ "/books/" (b.id) "/cover" } alt=""; }
            h1 { (b.title) }
            @if let Some(a) = &b.author { p .author { (a) } }
            p .meta { (b.format) @if let Some(s) = b.size { " · " (s / 1024) " KB" } }
            @if let Some(d) = &b.description { p .desc { (d) } }
            p {
                @if has_viewer { a .btn href={ "/read/" (b.id) } { "Read" } " " }
                a .btn href={ "/books/" (b.id) "/file" } { "Download " (b.format) }
            }
        }
    };
    Html(page(&b.title, body).into_string()).into_response()
}

/// Range-capable byte serving — the one seam every viewer needs. ServeFile gives
/// us Range/If-Range/content-type for free.
pub async fn book_file(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    req: Request<Body>,
) -> Response {
    let path: Option<String> = sqlx::query("SELECT path FROM books WHERE id=?")
        .bind(id)
        .fetch_optional(&state.pool)
        .await
        .ok()
        .flatten()
        .map(|r| r.get("path"));
    let Some(path) = path else {
        return (StatusCode::NOT_FOUND, "no such book").into_response();
    };
    match ServeFile::new(path).oneshot(req).await {
        Ok(res) => res.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

/// Serve a book's extracted cover image from DATA_DIR/covers.
pub async fn book_cover(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    req: Request<Body>,
) -> Response {
    let cover: Option<String> = sqlx::query("SELECT cover FROM books WHERE id=?")
        .bind(id)
        .fetch_optional(&state.pool)
        .await
        .ok()
        .flatten()
        .and_then(|r| r.get::<Option<String>, _>("cover"));
    let Some(name) = cover else {
        return (StatusCode::NOT_FOUND, "no cover").into_response();
    };
    match ServeFile::new(state.covers_dir.join(name)).oneshot(req).await {
        Ok(res) => res.into_response(),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

#[derive(Deserialize)]
pub struct DiscoverQ {
    pub q: Option<String>,
}

pub async fn discover(State(state): State<AppState>, Query(dq): Query<DiscoverQ>) -> Html<String> {
    let query = dq.q.unwrap_or_default();
    let mut candidates = vec![];
    if !query.trim().is_empty() {
        let sq = SearchQuery {
            text: query.clone(),
            format: None,
        };
        let sources = state.sources();
        for src in sources.values() {
            match src.search(&sq).await {
                Ok(mut c) => candidates.append(&mut c),
                Err(e) => tracing::warn!("search via {} failed: {e}", src.id()),
            }
        }
    }
    let body = html! {
        form .search action="/discover" method="get" {
            input type="search" name="q" value=(query) placeholder="Search Project Gutenberg…" autofocus;
            button type="submit" { "Search" }
        }
        @if !candidates.is_empty() {
            section .results {
                @for c in &candidates {
                    div .result {
                        span .title { (c.title) }
                        @if let Some(a) = &c.author { span .author { (a) } }
                        form hx-post="/add" hx-swap="outerHTML" hx-target="this" .addform {
                            input type="hidden" name="source" value=(c.source);
                            input type="hidden" name="title" value=(c.title);
                            @if let Some(a) = &c.author { input type="hidden" name="author" value=(a); }
                            input type="hidden" name="format" value=(c.format);
                            input type="hidden" name="reference" value=(c.reference);
                            button type="submit" .btn { "Add" }
                        }
                    }
                }
            }
        }
    };
    Html(page("Discover", body).into_string())
}

pub async fn add(State(state): State<AppState>, Form(cand): Form<Candidate>) -> Html<String> {
    let payload = serde_json::to_value(&cand).unwrap_or_default();
    match crate::jobs::enqueue(&state.pool, "download", &payload).await {
        Ok(_) => Html("<span class=\"queued\">Queued ✓</span>".into()),
        Err(e) => Html(format!("<span class=\"err\">Failed: {e}</span>")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_path_separators() {
        for evil in ["../../etc/passwd", "a/b\\c", "C:\\x", "..", "  ", ""] {
            let s = sanitize(evil);
            assert!(!s.contains('/'), "{evil:?} -> {s:?}");
            assert!(!s.contains('\\'), "{evil:?} -> {s:?}");
            assert!(!s.is_empty());
            assert_ne!(s, "..");
        }
        assert_eq!(sanitize("Pride and Prejudice"), "Pride and Prejudice");
    }

    #[test]
    fn format_detection() {
        assert_eq!(book_format(std::path::Path::new("x/Book.EPUB")).as_deref(), Some("epub"));
        assert_eq!(book_format(std::path::Path::new("x/scan.cbz")).as_deref(), Some("cbz"));
        assert_eq!(book_format(std::path::Path::new("x/notes.docx")), None);
        assert_eq!(book_format(std::path::Path::new("x/noext")), None);
    }
}
