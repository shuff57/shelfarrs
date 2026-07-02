//! OPDS 1.2 acquisition feed — a stable, read-only catalog every ebook reader
//! (Boox, KOReader, Thorium, …) can subscribe to. Core, not a plugin: it's the
//! one read surface every reader wants.

use crate::books::Book;
use crate::AppState;
use axum::{extract::State, http::header, response::IntoResponse};

fn mime_for(format: &str) -> &'static str {
    match format {
        "epub" => "application/epub+zip",
        "pdf" => "application/pdf",
        "cbz" => "application/x-cbz",
        "cbr" => "application/x-cbr",
        "mobi" => "application/x-mobipocket-ebook",
        "azw3" => "application/vnd.amazon.ebook",
        _ => "application/octet-stream",
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub async fn feed(State(state): State<AppState>) -> impl IntoResponse {
    let books: Vec<Book> = sqlx::query_as::<_, Book>(
        "SELECT id,title,author,format,size FROM books ORDER BY title",
    )
    .fetch_all(&state.pool)
    .await
    .unwrap_or_default();

    let mut x = String::new();
    x.push_str(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
    x.push_str(r#"<feed xmlns="http://www.w3.org/2005/Atom" xmlns:opds="http://opds-spec.org/2010/catalog">"#);
    x.push_str("<id>urn:shelfarrs:library</id>");
    x.push_str("<title>Shelfarrs Library</title>");
    x.push_str(r#"<link rel="self" href="/opds" type="application/atom+xml;profile=opds-catalog;kind=acquisition"/>"#);
    x.push_str(r#"<link rel="start" href="/opds" type="application/atom+xml;profile=opds-catalog;kind=acquisition"/>"#);
    for b in &books {
        let mime = mime_for(&b.format);
        x.push_str("<entry>");
        x.push_str(&format!("<id>urn:shelfarrs:book:{}</id>", b.id));
        x.push_str(&format!("<title>{}</title>", esc(&b.title)));
        if let Some(a) = &b.author {
            x.push_str(&format!("<author><name>{}</name></author>", esc(a)));
        }
        x.push_str(&format!("<dc:format xmlns:dc=\"http://purl.org/dc/terms/\">{}</dc:format>", b.format));
        x.push_str(&format!(
            r#"<link rel="http://opds-spec.org/acquisition" href="/books/{}/file" type="{}"/>"#,
            b.id, mime
        ));
        x.push_str("</entry>");
    }
    x.push_str("</feed>");

    (
        [(
            header::CONTENT_TYPE,
            "application/atom+xml;profile=opds-catalog;kind=acquisition",
        )],
        x,
    )
}
