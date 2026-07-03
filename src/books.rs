//! Library: model, filesystem scan, download→import, byte serving, and the
//! HTMX-rendered library / discover pages.

use crate::source::{Candidate, SearchQuery};
use crate::{page, AppState};
use anyhow::{Context, Result};
use axum::{
    body::Body,
    extract::{Path, Query, State},
    http::{Request, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
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
    pub series: Option<String>,
    pub series_index: Option<f64>,
    pub monitored: i64,
    pub isbn: Option<String>,
}

const BOOK_COLS: &str =
    "id,title,author,format,size,description,cover,series,series_index,monitored,isbn";

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
        sqlx::query_as::<_, Book>(&format!("SELECT {BOOK_COLS} FROM books ORDER BY title"))
            .fetch_all(&state.pool)
            .await?,
    )
}

// ---- HTTP handlers ----

#[derive(Deserialize)]
pub struct LibQ {
    pub group: Option<String>,
    pub view: Option<String>,
    pub info: Option<u8>,
    pub sort: Option<String>,
    pub dir: Option<String>,
}

/// One poster card (grid mode) — cover link, status underline, hover overlay
/// (title/author/monitored) and hover action buttons (read/edit/delete).
fn poster_card(b: &Book, info: bool) -> maud::Markup {
    html! {
        div .card {
            a .poster href={ "/books/" (b.id) } {
                @if b.cover.is_some() {
                    img .cover src={ "/books/" (b.id) "/cover" } alt="" loading="lazy";
                } @else {
                    div .cover-ph { span { (b.format) } }
                }
                div .overlay {
                    span .o-title { (b.title) }
                    @if let Some(a) = &b.author { span .o-author { (a) } }
                    span .monitored-badge {
                        span .icon { (maud::PreEscaped(crate::icons::EYE)) }
                        @if b.monitored == 1 { "Monitored" } @else { "Unmonitored" }
                    }
                }
            }
            div .card-actions {
                a .action-btn .play href={ "/read/" (b.id) } title="Read" { (maud::PreEscaped(crate::icons::BOOK_OPEN)) }
                a .action-btn href={ "/books/" (b.id) } title="Edit" { (maud::PreEscaped(crate::icons::PENCIL)) }
                form method="post" action={ "/books/" (b.id) "/delete" }
                    onsubmit={ "return confirm('Delete " (b.title.replace('\'', "\\'")) "?')" } .inline {
                    button .action-btn .del type="submit" title="Delete" { (maud::PreEscaped(crate::icons::TRASH)) }
                }
            }
            // captions only with the info toggle — bare posters by default, like the arr grid
            @if info {
                div .under {
                    span .title { (b.title) }
                    @if let Some(a) = &b.author { span .author { (a) } }
                    span .author { (b.format) @if let Some(s) = b.size { " · " (s / 1024) " KB" } }
                }
            }
        }
    }
}

/// The fixed bottom status legend, arr-style.
fn legend() -> maud::Markup {
    html! {
        footer .legend {
            span .li { span .dot style="background:#3498db" {} "Downloading" }
            span .li { span .dot style="background:#e74c3c" {} "Missing" }
            span .li { span .dot style="background:#2ecc71" {} "Downloaded" }
        }
    }
}

fn toolbar_url(group: &str, view: &str, info: bool, sort: &str, dir: &str) -> String {
    format!("/?group={group}&view={view}&info={}&sort={sort}&dir={dir}", info as u8)
}

pub async fn library(State(state): State<AppState>, Query(q): Query<LibQ>) -> Html<String> {
    let group = q.group.as_deref().unwrap_or("books").to_string();
    let view = q.view.as_deref().unwrap_or("grid").to_string();
    let info = q.info.unwrap_or(0) == 1;
    let sort = q.sort.as_deref().unwrap_or("title").to_string();
    let dir = q.dir.as_deref().unwrap_or("asc").to_string();

    let mut books = all_books(&state).await.unwrap_or_default();
    match sort.as_str() {
        "author" => books.sort_by(|a, b| a.author.cmp(&b.author)),
        "added" => books.sort_by_key(|b| -b.id),
        _ => {} // already title-ordered from SQL
    }
    if dir == "desc" {
        books.reverse();
    }

    let title = match group.as_str() {
        "authors" => "Authors",
        "series" => "Series",
        _ => "Library",
    };

    // toolbar (view toggle · info toggle · count · group dropdown · refresh | sort)
    let count_label = match group.as_str() {
        "authors" => {
            let mut set: Vec<&str> =
                books.iter().filter_map(|b| b.author.as_deref()).collect();
            set.sort();
            set.dedup();
            format!("{} Authors", set.len())
        }
        "series" => {
            let mut set: Vec<&str> = books.iter().filter_map(|b| b.series.as_deref()).collect();
            set.sort();
            set.dedup();
            format!("{} Series", set.len())
        }
        _ => format!("{} Book{}", books.len(), if books.len() == 1 { "" } else { "s" }),
    };
    let flip = |d: &str| if d == "asc" { "desc" } else { "asc" };
    let toolbar = html! {
        div .toolbar {
            div .toolbar-left {
                a .toolbar-btn .active[view == "grid"] title="Grid"
                    href=(toolbar_url(&group, "grid", info, &sort, &dir)) { span .icon { (maud::PreEscaped(crate::icons::GRID)) } }
                a .toolbar-btn .active[view == "list"] title="List"
                    href=(toolbar_url(&group, "list", info, &sort, &dir)) { span .icon { (maud::PreEscaped(crate::icons::LIST)) } }
                a .toolbar-btn .active[info] title="Details"
                    href=(toolbar_url(&group, &view, !info, &sort, &dir)) { span .icon { (maud::PreEscaped(crate::icons::INFO)) } }
                span .count-badge { (count_label) }
                details .menu {
                    summary .toolbar-btn {
                        @match group.as_str() { "authors" => "Authors", "series" => "Series", _ => "Books" }
                        span .icon { (maud::PreEscaped(crate::icons::CARET_DOWN)) }
                    }
                    div .dropdown {
                        a href=(toolbar_url("books", &view, info, &sort, &dir)) { "Books" }
                        a href=(toolbar_url("authors", &view, info, &sort, &dir)) { "Authors" }
                        a href=(toolbar_url("series", &view, info, &sort, &dir)) { "Series" }
                    }
                }
                form .inline action="/library-import/scan" method="post" {
                    button .toolbar-btn type="submit" { span .icon { (maud::PreEscaped(crate::icons::REFRESH)) } "Refresh" }
                }
            }
            div .toolbar-right {
                details .menu {
                    summary .toolbar-btn { "Sort: " (sort) span .icon { (maud::PreEscaped(crate::icons::CARET_DOWN)) } }
                    div .dropdown {
                        a href=(toolbar_url(&group, &view, info, "title", flip(&dir))) { "Title" }
                        a href=(toolbar_url(&group, &view, info, "author", flip(&dir))) { "Author" }
                        a href=(toolbar_url(&group, &view, info, "added", flip(&dir))) { "Added" }
                    }
                }
            }
        }
    };

    let content = if books.is_empty() {
        html! {
            div .empty-state {
                span .eicon { (maud::PreEscaped(crate::icons::BOOK_OPEN)) }
                p .empty-title { "Your library is empty" }
                p .empty-message { "Scan your books folder or add something new." }
                div .empty-actions { a .btn href="/add-new" { "Add New" } }
            }
        }
    } else {
        match group.as_str() {
            "authors" => {
                let mut authors: Vec<(String, Vec<&Book>)> = vec![];
                for b in &books {
                    let name = b.author.clone().unwrap_or_else(|| "Unknown".into());
                    match authors.iter_mut().find(|(n, _)| *n == name) {
                        Some((_, v)) => v.push(b),
                        None => authors.push((name, vec![b])),
                    }
                }
                authors.sort_by(|a, b| a.0.cmp(&b.0));
                html! {
                    section .grid {
                        @for (name, list) in &authors {
                            a .card href={ "/collection/author/" (urlenc(name)) } {
                                div .poster {
                                    @if let Some(c) = list.iter().find(|b| b.cover.is_some()) {
                                        img .cover src={ "/books/" (c.id) "/cover" } alt="" loading="lazy";
                                    } @else {
                                        div .cover-ph { span { "—" } }
                                    }
                                    div .overlay { span .o-title { (name) } }
                                    div .status-bar {}
                                }
                                span .title { (name) }
                                span .author { (list.len()) " book" @if list.len() != 1 { "s" } }
                            }
                        }
                    }
                }
            }
            "series" => {
                let mut series: Vec<(String, Vec<&Book>)> = vec![];
                for b in &books {
                    let Some(name) = b.series.clone() else { continue };
                    match series.iter_mut().find(|(n, _)| *n == name) {
                        Some((_, v)) => v.push(b),
                        None => series.push((name, vec![b])),
                    }
                }
                series.sort_by(|a, b| a.0.cmp(&b.0));
                for (_, list) in series.iter_mut() {
                    list.sort_by(|a, b| {
                        a.series_index
                            .unwrap_or(f64::MAX)
                            .total_cmp(&b.series_index.unwrap_or(f64::MAX))
                    });
                }
                if series.is_empty() {
                    html! {
                        div .empty-state {
                            span .eicon { (maud::PreEscaped(crate::icons::BOOKS)) }
                            p .empty-title { "No series information" }
                            p .empty-message { "Books whose epub metadata names a series will group here after a scan." }
                        }
                    }
                } else {
                    html! {
                        section .grid {
                            @for (name, list) in &series {
                                a .card href={ "/collection/series/" (urlenc(name)) } {
                                    div .poster {
                                        @if let Some(c) = list.iter().find(|b| b.cover.is_some()) {
                                            img .cover src={ "/books/" (c.id) "/cover" } alt="" loading="lazy";
                                        } @else {
                                            div .cover-ph { span { "—" } }
                                        }
                                        div .overlay { span .o-title { (name) } }
                                        div .status-bar {}
                                    }
                                    span .title { (name) }
                                    span .author { (list.len()) " book" @if list.len() != 1 { "s" } }
                                }
                            }
                        }
                    }
                }
            }
            _ if view == "list" => html! {
                form method="post" action="/books/bulk" {
                    div .bulkbar {
                        button .toolbar-btn type="submit" name="action" value="monitor" { "Monitor" }
                        button .toolbar-btn type="submit" name="action" value="unmonitor" { "Unmonitor" }
                        button .toolbar-btn .danger-text type="submit" name="action" value="delete" { "Delete" }
                        label .check { input type="checkbox" name="delete_files" value="1"; " also delete files" }
                    }
                    table .arr-table {
                        thead { tr { th {} th {} th { "Title" } th { "Author" } th { "Format" } th { "Size" } th {} } }
                        tbody {
                            @for b in &books {
                                tr {
                                    td { input type="checkbox" name="ids" value=(b.id); }
                                    td .t-thumb {
                                        @if b.cover.is_some() { img src={ "/books/" (b.id) "/cover" } alt=""; }
                                    }
                                    td .t-title { a href={ "/books/" (b.id) } { (b.title) } }
                                    td .t-muted { (b.author.clone().unwrap_or_default()) }
                                    td .t-muted { (b.format) }
                                    td .t-muted { @if let Some(s) = b.size { (s / 1024) " KB" } }
                                    td { a .btn .btn-sm href={ "/read/" (b.id) } { "Read" } }
                                }
                            }
                        }
                    }
                }
            },
            _ => html! {
                section .grid {
                    @for b in &books { (poster_card(b, info)) }
                }
            },
        }
    };

    let body = html! { (toolbar) (content) (legend()) };
    Html(page(title, body).into_string())
}

/// Author (or later: series) collection — hero header + that group's books.
pub async fn collection(
    State(state): State<AppState>,
    Path((kind, name)): Path<(String, String)>,
) -> Html<String> {
    let mut books: Vec<Book> = all_books(&state)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|b| match kind.as_str() {
            "author" => b.author.as_deref() == Some(name.as_str()),
            "series" => b.series.as_deref() == Some(name.as_str()),
            _ => false,
        })
        .collect();
    if kind == "series" {
        books.sort_by(|a, b| {
            a.series_index
                .unwrap_or(f64::MAX)
                .total_cmp(&b.series_index.unwrap_or(f64::MAX))
        });
    }
    let body = html! {
        section .hero {
            h1 { (name) }
            p .muted { (books.len()) " book" @if books.len() != 1 { "s" } " in library" }
        }
        section .grid {
            @for b in &books {
                @if kind == "series" {
                    div .seq-card {
                        (poster_card(b, false))
                        @if let Some(i) = b.series_index { span .seq { "#" (fmt_index(i)) } }
                    }
                } @else { (poster_card(b, false)) }
            }
        }
        (legend())
    };
    Html(page("Collection", body).into_string())
}

/// "2" for 2.0, "2.5" for halves — series indexes are REAL for novellas.
fn fmt_index(i: f64) -> String {
    if i.fract() == 0.0 {
        format!("{}", i as i64)
    } else {
        format!("{i}")
    }
}

/// Month calendar of when books were added to the library.
#[derive(Deserialize)]
pub struct CalQ {
    pub m: Option<String>, // YYYY-MM
}

pub async fn calendar(State(state): State<AppState>, Query(q): Query<CalQ>) -> Html<String> {
    // Current month from the DB clock (no chrono dep).
    let now: String = sqlx::query_scalar("SELECT strftime('%Y-%m','now')")
        .fetch_one(&state.pool)
        .await
        .unwrap_or_else(|_| "1970-01".into());
    let month = q.m.unwrap_or(now.clone());
    let (prev, next): (String, String) = {
        let p = sqlx::query_scalar("SELECT strftime('%Y-%m', ? || '-01', '-1 month')")
            .bind(&month)
            .fetch_one(&state.pool)
            .await
            .unwrap_or_else(|_| month.clone());
        let n = sqlx::query_scalar("SELECT strftime('%Y-%m', ? || '-01', '+1 month')")
            .bind(&month)
            .fetch_one(&state.pool)
            .await
            .unwrap_or_else(|_| month.clone());
        (p, n)
    };
    let days: i64 = sqlx::query_scalar("SELECT CAST(strftime('%d', ? || '-01', '+1 month', '-1 day') AS INTEGER)")
        .bind(&month)
        .fetch_one(&state.pool)
        .await
        .unwrap_or(30);
    // weekday of the 1st (0=Sunday)
    let start: i64 = sqlx::query_scalar("SELECT CAST(strftime('%w', ? || '-01') AS INTEGER)")
        .bind(&month)
        .fetch_one(&state.pool)
        .await
        .unwrap_or(0);
    let rows = sqlx::query(
        "SELECT id, title, CAST(strftime('%d', added_at) AS INTEGER) AS day
         FROM books WHERE strftime('%Y-%m', added_at) = ? ORDER BY added_at",
    )
    .bind(&month)
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default();

    let body = html! {
        header .pagehead {
            span .ph-icon { (maud::PreEscaped(crate::icons::CALENDAR)) }
            h1 { "Calendar" }
            span .count-badge { (month) }
            div .headbtns {
                a .toolbar-btn href={ "/calendar?m=" (prev) } { "‹ Prev" }
                a .toolbar-btn href="/calendar" { "Today" }
                a .toolbar-btn href={ "/calendar?m=" (next) } { "Next ›" }
            }
        }
        div .calgrid {
            @for d in ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"] { div .caldow { (d) } }
            @for _ in 0..start { div .calcell .off {} }
            @for day in 1..=days {
                div .calcell {
                    span .daynum { (day) }
                    @for r in &rows {
                        @if r.get::<i64, _>("day") == day {
                            a .calitem href={ "/books/" (r.get::<i64, _>("id")) } { (r.get::<String, _>("title")) }
                        }
                    }
                }
            }
        }
    };
    Html(page("Calendar", body).into_string())
}

/// Library Import: the scan surface — root folder + Scan Now + recent scans.
pub async fn library_import(State(state): State<AppState>) -> Html<String> {
    let scans = sqlx::query(
        "SELECT status, updated_at FROM jobs WHERE kind='scan' ORDER BY id DESC LIMIT 5",
    )
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default();
    let body = html! {
        header .pagehead {
            span .ph-icon { (maud::PreEscaped(crate::icons::FOLDER_OPEN)) }
            h1 { "Library Import" }
        }
        p .muted { "Import existing organized ebooks from your books folder." }
        div .cards {
            div .statcard {
                span .k { "Root folder" }
                span .v { code { (state.books_dir.display()) } }
            }
        }
        form action="/library-import/scan" method="post" style="margin-top:1rem" {
            button .btn type="submit" { "Scan Now" }
        }
        @let pattern = naming_pattern(&state).await;
        @let plan = organize_plan(&state).await;
        h2 { "Naming & organize" }
        form .search action="/library-import/naming" method="post" {
            input type="text" name="pattern" value=(pattern);
            button .btn type="submit" { "Save pattern" }
        }
        p .muted { "Tokens: " code { "{Author} {Series} {SeriesNumber} {Title}" } " — '/' makes folders. Segments whose tokens are all empty are dropped." }
        @if plan.is_empty() {
            p .muted { "Everything already matches the pattern." }
        } @else {
            table .arr-table {
                thead { tr { th { "Current" } th { "Target" } } }
                tbody {
                    @for (_, cur, tgt) in plan.iter().take(20) {
                        tr {
                            td .t-muted { (cur) }
                            td .t-title { (tgt.display()) }
                        }
                    }
                }
            }
            form .inline hx-post="/library-import/organize" hx-target="this" hx-swap="outerHTML" style="margin-top:.75rem;display:block" {
                button .btn type="submit" { "Organize " (plan.len()) " file(s)" }
            }
        }
        h2 { "Recent scans" }
        @if scans.is_empty() { p .muted { "No scans yet." } }
        div .results {
            @for s in &scans {
                @let status: String = s.get("status");
                div .result {
                    span .title { "Library scan" }
                    span .author { (s.get::<String, _>("updated_at")) }
                    span .status-badge .completed[status == "done"] .failed[status == "failed"] .queued[status == "queued" || status == "running"] { (status) }
                }
            }
        }
    };
    Html(page("Library Import", body).into_string())
}

pub async fn scan_now(State(state): State<AppState>) -> axum::response::Redirect {
    let _ = crate::jobs::enqueue(&state.pool, "scan", &serde_json::json!({})).await;
    axum::response::Redirect::to("/activity")
}

/// Topbar live search suggestions (HTMX).
#[derive(Deserialize)]
pub struct SuggestQ {
    pub q: Option<String>,
}

pub async fn search_suggest(State(state): State<AppState>, Query(sq): Query<SuggestQ>) -> Html<String> {
    let q = sq.q.unwrap_or_default();
    if q.trim().is_empty() {
        return Html(String::new());
    }
    let rows = sqlx::query_as::<_, Book>(&format!(
        "SELECT {BOOK_COLS} FROM books
         WHERE title LIKE '%'||?||'%' OR author LIKE '%'||?||'%' ORDER BY title LIMIT 6"
    ))
    .bind(&q)
    .bind(&q)
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default();
    let m = html! {
        @for b in &rows {
            a .sug href={ "/books/" (b.id) } {
                @if b.cover.is_some() { img src={ "/books/" (b.id) "/cover" } alt=""; }
                    @else { span .thumb-ph {} }
                span .s-title { (b.title) }
                @if let Some(a) = &b.author { span .s-author { (a) } }
            }
        }
        @if rows.is_empty() { p .muted style="padding:.5rem .8rem" { "No matches" } }
    };
    Html(m.into_string())
}

// ---- B6: naming pattern + organize ----

pub const DEFAULT_PATTERN: &str = "{Author}/{Title}";

/// Render a naming pattern into a relative path. Tokens: {Author} {Series}
/// {SeriesNumber} {Title}. A path segment whose tokens all came up empty is
/// dropped (no stray folders for books without a series).
pub fn render_pattern(pattern: &str, b: &Book) -> PathBuf {
    let seriesnum = b.series_index.map(fmt_index).unwrap_or_default();
    let mut out = PathBuf::new();
    for seg in pattern.split('/') {
        let had_token = seg.contains('{');
        let rendered = seg
            .replace("{Author}", b.author.as_deref().unwrap_or(""))
            .replace("{Series}", b.series.as_deref().unwrap_or(""))
            .replace("{SeriesNumber}", &seriesnum)
            .replace("{Title}", &b.title);
        let clean = sanitize(rendered.trim());
        if had_token && (clean.is_empty() || clean == "book") && rendered.trim().is_empty() {
            continue; // all tokens empty -> drop segment
        }
        if !rendered.trim().is_empty() {
            out.push(clean);
        }
    }
    if out.as_os_str().is_empty() {
        out.push(sanitize(&b.title));
    }
    out
}

pub async fn naming_pattern(state: &AppState) -> String {
    sqlx::query("SELECT value FROM settings WHERE key='naming_pattern'")
        .fetch_optional(&state.pool)
        .await
        .ok()
        .flatten()
        .map(|r| r.get::<String, _>("value"))
        .filter(|p| !p.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_PATTERN.into())
}

/// (book, current path, target path) for every book not already organized.
pub async fn organize_plan(state: &AppState) -> Vec<(i64, String, PathBuf)> {
    let pattern = naming_pattern(state).await;
    let books = all_books(state).await.unwrap_or_default();
    let mut out = vec![];
    for b in &books {
        let path: Option<String> = sqlx::query("SELECT path FROM books WHERE id=?")
            .bind(b.id)
            .fetch_optional(&state.pool)
            .await
            .ok()
            .flatten()
            .map(|r| r.get("path"));
        let Some(current) = path else { continue };
        let target = state
            .books_dir
            .join(render_pattern(&pattern, b))
            .with_extension(&b.format);
        if std::path::Path::new(&current) != target.as_path() {
            out.push((b.id, current, target));
        }
    }
    out
}

#[derive(Deserialize)]
pub struct PatternForm {
    pub pattern: String,
}

pub async fn naming_save(
    State(state): State<AppState>,
    axum::Extension(me): axum::Extension<crate::auth::CurrentUser>,
    Form(f): Form<PatternForm>,
) -> Response {
    if let Some(r) = crate::auth::deny_non_admin(&me) {
        return r;
    }
    let _ = sqlx::query(
        "INSERT INTO settings (key,value) VALUES ('naming_pattern',?)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
    )
    .bind(f.pattern.trim())
    .execute(&state.pool)
    .await;
    Redirect::to("/library-import").into_response()
}

/// Execute the organize plan: move files into the pattern layout.
pub async fn organize_apply(
    State(state): State<AppState>,
    axum::Extension(me): axum::Extension<crate::auth::CurrentUser>,
) -> Response {
    if let Some(r) = crate::auth::deny_non_admin(&me) {
        return r;
    }
    let plan = organize_plan(&state).await;
    let mut moved = 0;
    for (id, current, target) in plan {
        if let Some(parent) = target.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match std::fs::rename(&current, &target) {
            Ok(_) => {
                let _ = sqlx::query("UPDATE books SET path=? WHERE id=?")
                    .bind(target.to_string_lossy().as_ref())
                    .bind(id)
                    .execute(&state.pool)
                    .await;
                moved += 1;
            }
            Err(e) => tracing::warn!("organize: could not move {current}: {e}"),
        }
    }
    Html(format!("<span class=\"queued\">Moved {moved} file(s) ✓</span>")).into_response()
}

// ---- B6: bulk edit (list view) ----

/// Bulk actions from the list view. Parsed by hand — repeated `ids=` keys
/// aren't supported by serde_urlencoded and don't justify a dependency.
pub async fn bulk(
    State(state): State<AppState>,
    axum::Extension(me): axum::Extension<crate::auth::CurrentUser>,
    body: String,
) -> Response {
    if let Some(r) = crate::auth::deny_non_admin(&me) {
        return r;
    }
    let mut ids: Vec<i64> = vec![];
    let mut action = String::new();
    let mut delete_files = false;
    for pair in body.split('&') {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        match k {
            "ids" => {
                if let Ok(id) = v.parse() {
                    ids.push(id);
                }
            }
            "action" => action = v.to_string(),
            "delete_files" => delete_files = true,
            _ => {}
        }
    }
    for id in &ids {
        match action.as_str() {
            "monitor" => {
                let _ = sqlx::query("UPDATE books SET monitored=1 WHERE id=?").bind(id).execute(&state.pool).await;
            }
            "unmonitor" => {
                let _ = sqlx::query("UPDATE books SET monitored=0 WHERE id=?").bind(id).execute(&state.pool).await;
            }
            "delete" => {
                // reuse the single-book path (cover/progress cleanup + confinement)
                let _ = delete_one(&state, *id, delete_files).await;
            }
            _ => {}
        }
    }
    Redirect::to("/?view=list").into_response()
}

async fn delete_one(state: &AppState, id: i64, delete_file: bool) -> anyhow::Result<()> {
    let row = sqlx::query("SELECT path, cover FROM books WHERE id=?")
        .bind(id)
        .fetch_optional(&state.pool)
        .await?
        .context("no such book")?;
    if delete_file {
        let path: String = row.get("path");
        let p = std::path::Path::new(&path);
        if p.starts_with(&state.books_dir) {
            let _ = std::fs::remove_file(p);
        }
    }
    if let Some(cover) = row.get::<Option<String>, _>("cover") {
        let _ = std::fs::remove_file(state.covers_dir.join(cover));
    }
    sqlx::query("DELETE FROM progress WHERE book=?").bind(id).execute(&state.pool).await?;
    sqlx::query("DELETE FROM books WHERE id=?").bind(id).execute(&state.pool).await?;
    Ok(())
}

pub(crate) fn urlenc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[derive(Deserialize)]
pub struct DetailQ {
    pub tab: Option<String>,
}

pub async fn book_detail(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(dq): Query<DetailQ>,
) -> Response {
    let book = sqlx::query_as::<_, Book>(&format!("SELECT {BOOK_COLS} FROM books WHERE id=?"))
    .bind(id)
    .fetch_optional(&state.pool)
    .await
    .ok()
    .flatten();
    let Some(b) = book else {
        return (StatusCode::NOT_FOUND, "no such book").into_response();
    };
    let has_viewer = crate::plugin::viewer_for(&state.plugins_dir, &b.format).is_some();
    let tab = dq.tab.as_deref().unwrap_or("details");
    let file_row = sqlx::query("SELECT path, added_at FROM books WHERE id=?")
        .bind(id)
        .fetch_optional(&state.pool)
        .await
        .ok()
        .flatten();
    let (path, added): (String, String) = file_row
        .map(|r| (r.get("path"), r.get("added_at")))
        .unwrap_or_default();
    let history = sqlx::query(
        "SELECT title, state, updated_at FROM downloads WHERE title LIKE '%'||?||'%' ORDER BY id DESC LIMIT 20",
    )
    .bind(&b.title)
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default();
    let cover_url = format!("/books/{}/cover", b.id);

    let body = html! {
        div .detail-nav {
            a .toolbar-btn href="/" { span .icon { (maud::PreEscaped(crate::icons::ARROW_LEFT)) } "Back" }
            div .detail-actions {
                @if has_viewer { a .action-btn .play .lg href={ "/read/" (b.id) } title="Read" { (maud::PreEscaped(crate::icons::BOOK_OPEN)) } }
                a .action-btn .lg href={ "/books/" (b.id) "/file" } title="Download" { (maud::PreEscaped(crate::icons::DOWNLOAD)) }
                form .inline hx-post={ "/books/" (b.id) "/rescan" } hx-swap="none" {
                    button .action-btn .lg type="submit" title="Rescan metadata" { (maud::PreEscaped(crate::icons::REFRESH)) }
                }
                form .inline hx-post={ "/books/" (b.id) "/auto-search" } hx-swap="none" {
                    button .action-btn .lg type="submit" title="Search for upgrade" { (maud::PreEscaped(crate::icons::ROBOT)) }
                }
                form method="post" action={ "/books/" (b.id) "/delete" }
                    onsubmit="return confirm('Delete this book?')" .inline {
                    button .action-btn .del .lg type="submit" title="Delete" { (maud::PreEscaped(crate::icons::TRASH)) }
                }
            }
        }
        section .hero-detail {
            @if b.cover.is_some() {
                div .hero-bg style={ "background-image:url('" (cover_url) "')" } {}
            }
            div .hero-inner {
                @if b.cover.is_some() { img .hero-cover src=(cover_url) alt=""; }
                    @else { div .hero-cover .cover-ph { span { (b.format) } } }
                div .hero-info {
                    h1 { (b.title) }
                    @if let Some(s) = &b.series {
                        p .series-line {
                            a href={ "/collection/series/" (urlenc(s)) } { (s) }
                            @if let Some(i) = b.series_index { ", Book " (fmt_index(i)) }
                        }
                    }
                    @if let Some(a) = &b.author { p .author { (a) } }
                    div .path-chip {
                        span .icon { (maud::PreEscaped(crate::icons::FOLDER)) }
                        span { (path) }
                    }
                    div .chip-row {
                        span .chip .outline .on[b.monitored == 1] {
                            span .icon { (maud::PreEscaped(crate::icons::EYE)) }
                            @if b.monitored == 1 { "Monitored" } @else { "Unmonitored" }
                        }
                        span .chip .outline { (b.format) }
                        @if let Some(s) = b.size { span .chip .outline { (s / 1024) " KB" } }
                        @if let Some(i) = &b.isbn { span .chip .outline { "ISBN " (i) } }
                    }
                    @if let Some(d) = &b.description {
                        input #desc-more type="checkbox" hidden;
                        p .desc { (d) }
                        @if d.len() > 350 { label .toolbar-btn .show-more for="desc-more" { span .more { "Show More" } span .less { "Show Less" } } }
                    }
                }
            }
        }
        div .tabs {
            a .tab .active[tab == "details"] href={ "/books/" (b.id) "?tab=details" } { "Details" }
            a .tab .active[tab == "files"] href={ "/books/" (b.id) "?tab=files" } { "Files" }
            a .tab .active[tab == "history"] href={ "/books/" (b.id) "?tab=history" } { "History" }
        }
        @match tab {
            "files" => {
                table .arr-table {
                    thead { tr { th { "Path" } th { "Format" } th { "Size" } th { "Added" } } }
                    tbody { tr {
                        td .t-muted { (path) }
                        td { span .chip { (b.format) } }
                        td .t-muted { @if let Some(s) = b.size { (s / 1024) " KB" } }
                        td .t-muted { (added) }
                    } }
                }
            }
            "history" => {
                @if history.is_empty() { p .muted { "No download history for this book." } }
                @else {
                    table .arr-table {
                        thead { tr { th { "Release" } th { "Updated" } th { "Status" } } }
                        tbody {
                            @for h in &history {
                                @let st: String = h.get("state");
                                tr {
                                    td .t-title { (h.get::<String, _>("title")) }
                                    td .t-muted { (h.get::<String, _>("updated_at")) }
                                    td { span .status-badge .completed[st == "imported"] .failed[st == "failed"] .downloading[st != "imported" && st != "failed"] { (st) } }
                                }
                            }
                        }
                    }
                }
            }
            _ => {
                div .cards {
                    div .statcard {
                        span .k { "Author" }
                        span .v { @if let Some(a) = &b.author { a href={ "/collection/author/" (urlenc(a)) } { (a) } } @else { "Unknown" } }
                    }
                    div .statcard {
                        span .k { "Publication" }
                        span .v { (b.format) @if let Some(s) = b.size { " · " (s / 1024) " KB" } @if let Some(i) = &b.isbn { " · ISBN " (i) } }
                    }
                    div .statcard {
                        span .k { "Series" }
                        span .v {
                            @if let Some(s) = &b.series {
                                a href={ "/collection/series/" (urlenc(s)) } { (s) }
                                @if let Some(i) = b.series_index { " #" (fmt_index(i)) }
                            } @else { "—" }
                        }
                    }
                    div .statcard { span .k { "Added" } span .v { (added) } }
                }
            }
        }
        article .detail {

            details .panel {
                summary .toolbar-btn { "Edit" }
                form .editform action={ "/books/" (b.id) "/edit" } method="post" {
                    label { "Title" } input type="text" name="title" value=(b.title);
                    label { "Author" } input type="text" name="author" value=[b.author.as_deref()];
                    label { "Series" } input type="text" name="series" value=[b.series.as_deref()];
                    label { "Series #" } input type="number" step="0.1" name="series_index"
                        value=[b.series_index.map(fmt_index)];
                    label { "ISBN" } input type="text" name="isbn" value=[b.isbn.as_deref()];
                    label { "Description" } textarea .ta name="description" rows="4" {
                        (b.description.clone().unwrap_or_default())
                    }
                    label .check {
                        input type="checkbox" name="monitored" value="1" checked[b.monitored == 1];
                        " Monitored"
                    }
                    button .btn type="submit" { "Save" }
                }
            }
            details .panel {
                summary .toolbar-btn .danger-text { "Delete" }
                form .editform action={ "/books/" (b.id) "/delete" } method="post" {
                    label .check {
                        input type="checkbox" name="delete_file" value="1";
                        " Also delete the file from disk"
                    }
                    button .btn .danger type="submit" { "Delete book" }
                }
            }
        }
    };
    Html(page(&b.title, body).into_string()).into_response()
}

// ---- B1: edit / delete ----

#[derive(Deserialize)]
pub struct EditForm {
    pub title: Option<String>,
    pub author: Option<String>,
    pub series: Option<String>,
    pub series_index: Option<String>,
    pub isbn: Option<String>,
    pub description: Option<String>,
    pub monitored: Option<String>, // checkbox: present = on
}

/// Patch-style edit: blank fields clear optionals, blank title keeps the old one.
pub async fn book_edit(
    State(state): State<AppState>,
    axum::Extension(me): axum::Extension<crate::auth::CurrentUser>,
    Path(id): Path<i64>,
    Form(f): Form<EditForm>,
) -> Response {
    if let Some(r) = crate::auth::deny_non_admin(&me) {
        return r;
    }
    let none_if_blank = |s: Option<String>| s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let idx: Option<f64> = none_if_blank(f.series_index).and_then(|s| s.parse().ok());
    let _ = sqlx::query(
        "UPDATE books SET title=COALESCE(?,title), author=?, series=?, series_index=?,
         isbn=?, description=?, monitored=? WHERE id=?",
    )
    .bind(none_if_blank(f.title))
    .bind(none_if_blank(f.author))
    .bind(none_if_blank(f.series))
    .bind(idx)
    .bind(none_if_blank(f.isbn))
    .bind(none_if_blank(f.description))
    .bind(if f.monitored.is_some() { 1i64 } else { 0 })
    .bind(id)
    .execute(&state.pool)
    .await;
    Redirect::to(&format!("/books/{id}")).into_response()
}

/// Re-match: clear derived metadata and re-run extraction + providers.
pub async fn book_rescan(
    State(state): State<AppState>,
    axum::Extension(me): axum::Extension<crate::auth::CurrentUser>,
    Path(id): Path<i64>,
) -> Response {
    if let Some(r) = crate::auth::deny_non_admin(&me) {
        return r;
    }
    let _ = sqlx::query(
        "UPDATE books SET description=NULL, cover=NULL, series=NULL, series_index=NULL,
         isbn=NULL, meta_done=0 WHERE id=?",
    )
    .bind(id)
    .execute(&state.pool)
    .await;
    let n = crate::meta::enrich_pending(&state).await;
    Html(format!("<span class=\"queued\">Rescanned ({n}) ✓ — reload to see changes</span>"))
        .into_response()
}

#[derive(Deserialize)]
pub struct DeleteForm {
    pub delete_file: Option<String>,
}

pub async fn book_delete(
    State(state): State<AppState>,
    axum::Extension(me): axum::Extension<crate::auth::CurrentUser>,
    Path(id): Path<i64>,
    Form(f): Form<DeleteForm>,
) -> Response {
    if let Some(r) = crate::auth::deny_non_admin(&me) {
        return r;
    }
    let row = sqlx::query("SELECT path, cover FROM books WHERE id=?")
        .bind(id)
        .fetch_optional(&state.pool)
        .await
        .ok()
        .flatten();
    if let Some(r) = row {
        if f.delete_file.is_some() {
            let path: String = r.get("path");
            // Only ever delete inside the books dir (defence-in-depth).
            let p = std::path::Path::new(&path);
            if p.starts_with(&state.books_dir) {
                let _ = std::fs::remove_file(p);
            } else {
                tracing::warn!("refusing to delete outside books dir: {path}");
            }
        }
        if let Some(cover) = r.get::<Option<String>, _>("cover") {
            let _ = std::fs::remove_file(state.covers_dir.join(cover));
        }
        let _ = sqlx::query("DELETE FROM progress WHERE book=?").bind(id).execute(&state.pool).await;
        let _ = sqlx::query("DELETE FROM books WHERE id=?").bind(id).execute(&state.pool).await;
        tracing::info!("deleted book {id} (file: {})", f.delete_file.is_some());
    }
    Redirect::to("/").into_response()
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
    // Titles already in the library — their result cards show "Added".
    let have: Vec<String> = sqlx::query_scalar("SELECT LOWER(title) FROM books")
        .fetch_all(&state.pool)
        .await
        .unwrap_or_default();
    let releases = if query.trim().is_empty() {
        vec![]
    } else {
        crate::indexer::search_all(&state, &query).await
    };
    let body = html! {
        header .pagehead {
            span .ph-icon { (maud::PreEscaped(crate::icons::PLUS)) }
            h1 { "Add New Book" }
        }
        form .search .addsearch action="/add-new" method="get" {
            input type="search" name="q" value=(query) placeholder="Search by title or author…" autofocus;
            button .btn type="submit" { span .icon { (maud::PreEscaped(crate::icons::SEARCH)) } "Search" }
        }
        @if query.trim().is_empty() {
            div .empty-state {
                span .eicon { (maud::PreEscaped(crate::icons::SEARCH)) }
                p .empty-title { "Search for a new book" }
                p .empty-message { "Results come from your source plugins and configured indexers." }
            }
        } @else if candidates.is_empty() && releases.is_empty() {
            div .empty-state {
                span .eicon { (maud::PreEscaped(crate::icons::BOOK_OPEN)) }
                p .empty-title { "No results" }
                p .empty-message { "Try a different title or author." }
            }
        } @else {
            @if !candidates.is_empty() { h2 { "Direct sources" } }
            section .result-cards {
                @for c in &candidates {
                    div .result-card {
                        div .result-info {
                            h3 { (c.title) }
                            @if let Some(a) = &c.author { p .by { "by " (a) } }
                            div .badges {
                                span .chip { (c.format) }
                                span .chip { (c.source) }
                            }
                        }
                        div .result-actions {
                            @if have.contains(&c.title.to_lowercase()) {
                                span .btn .btn-success .added { "✓ Added" }
                            } @else {
                                form hx-post="/add" hx-swap="outerHTML" hx-target="this" .addform {
                                    input type="hidden" name="source" value=(c.source);
                                    input type="hidden" name="title" value=(c.title);
                                    @if let Some(a) = &c.author { input type="hidden" name="author" value=(a); }
                                    input type="hidden" name="format" value=(c.format);
                                    input type="hidden" name="reference" value=(c.reference);
                                    button type="submit" .btn { "+ Add to Library" }
                                }
                            }
                        }
                    }
                }
            }
            @if !releases.is_empty() {
                h2 { "Releases (" (releases.len()) ")" }
                table .arr-table {
                    thead { tr { th { "Title" } th { "Indexer" } th { "Protocol" } th { "Size" } th { "Seeders" } th {} } }
                    tbody {
                        @for r in &releases {
                            tr {
                                td .t-title { (r.title) }
                                td .t-muted { (r.indexer) }
                                td { span .chip { (r.protocol) } }
                                td .t-muted { @if let Some(s) = r.size { (crate::indexer::fmt_size(s)) } @else { "—" } }
                                td .t-muted { @if let Some(s) = r.seeders { (s) } @else { "—" } }
                                td {
                                    form hx-post="/grab" hx-swap="outerHTML" hx-target="this" .addform {
                                        input type="hidden" name="payload" value=(serde_json::to_string(r).unwrap_or_default());
                                        button type="submit" .btn .btn-sm { "Grab" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    };
    Html(page("Add New", body).into_string())
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
    fn pattern_rendering() {
        let b = |author: Option<&str>, series: Option<&str>, idx: Option<f64>| Book {
            id: 1,
            title: "The Title".into(),
            author: author.map(String::from),
            format: "epub".into(),
            size: None,
            description: None,
            cover: None,
            series: series.map(String::from),
            series_index: idx,
            monitored: 1,
            isbn: None,
        };
        let p = "{Author}/{Series}/{SeriesNumber} - {Title}";
        assert_eq!(
            render_pattern(p, &b(Some("A. Writer"), Some("Saga"), Some(2.0))),
            std::path::Path::new("A. Writer").join("Saga").join("2 - The Title")
        );
        // no series -> series segment dropped, no stray "-" folder
        let r = render_pattern("{Author}/{Series}/{Title}", &b(Some("A. Writer"), None, None));
        assert_eq!(r, std::path::Path::new("A. Writer").join("The Title"));
        // path-hostile characters sanitized
        let evil = render_pattern("{Title}", &b(None, None, None));
        assert!(!evil.to_string_lossy().contains(".."));
    }

    #[test]
    fn format_detection() {
        assert_eq!(book_format(std::path::Path::new("x/Book.EPUB")).as_deref(), Some("epub"));
        assert_eq!(book_format(std::path::Path::new("x/scan.cbz")).as_deref(), Some("cbz"));
        assert_eq!(book_format(std::path::Path::new("x/notes.docx")), None);
        assert_eq!(book_format(std::path::Path::new("x/noext")), None);
    }
}
