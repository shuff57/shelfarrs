//! B5: online metadata providers — OpenLibrary (free, ISBN-first) and Google
//! Books (better descriptions). File-embedded extraction stays first; these
//! fill what the file lacks and power per-book rescans. Parsing is pure so
//! it's unit-testable without the network.

use crate::AppState;
use serde_json::Value;
use sqlx::Row;

#[derive(Debug, Default, PartialEq)]
pub struct ProviderMeta {
    pub title: Option<String>,
    pub author: Option<String>,
    pub description: Option<String>,
    pub cover_url: Option<String>,
    pub isbn: Option<String>,
}

// ---- settings toggles (KV; default on) ----

pub async fn enabled(state: &AppState, key: &str) -> bool {
    let v: Option<String> = sqlx::query("SELECT value FROM settings WHERE key=?")
        .bind(key)
        .fetch_optional(&state.pool)
        .await
        .ok()
        .flatten()
        .map(|r| r.get("value"));
    v.as_deref() != Some("0")
}

// ---- OpenLibrary ----

pub fn parse_openlibrary(search: &Value) -> Option<ProviderMeta> {
    let doc = search.get("docs")?.as_array()?.first()?;
    Some(ProviderMeta {
        title: doc.get("title").and_then(|t| t.as_str()).map(String::from),
        author: doc
            .pointer("/author_name/0")
            .and_then(|a| a.as_str())
            .map(String::from),
        description: None, // needs a second work fetch
        cover_url: doc
            .get("cover_i")
            .and_then(|c| c.as_i64())
            .map(|id| format!("https://covers.openlibrary.org/b/id/{id}-L.jpg")),
        isbn: doc.pointer("/isbn/0").and_then(|i| i.as_str()).map(String::from),
    })
}

/// dc:description lives on the work record; it's either a string or {value}.
pub fn parse_ol_work_description(work: &Value) -> Option<String> {
    match work.get("description") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(o) => o.get("value").and_then(|v| v.as_str()).map(String::from),
        None => None,
    }
}

async fn openlibrary(state: &AppState, title: &str, author: Option<&str>) -> Option<ProviderMeta> {
    let mut url = format!(
        "https://openlibrary.org/search.json?limit=3&title={}",
        crate::books::urlenc(title)
    );
    if let Some(a) = author {
        url.push_str(&format!("&author={}", crate::books::urlenc(a)));
    }
    let v: Value = state.http.get(&url).send().await.ok()?.json().await.ok()?;
    let mut meta = parse_openlibrary(&v)?;
    // work key -> description
    if let Some(key) = v.pointer("/docs/0/key").and_then(|k| k.as_str()) {
        if let Ok(resp) = state
            .http
            .get(format!("https://openlibrary.org{key}.json"))
            .send()
            .await
        {
            if let Ok(w) = resp.json::<Value>().await {
                meta.description = parse_ol_work_description(&w);
            }
        }
    }
    Some(meta)
}

// ---- Google Books ----

pub fn parse_googlebooks(v: &Value) -> Option<ProviderMeta> {
    let info = v.pointer("/items/0/volumeInfo")?;
    Some(ProviderMeta {
        title: info.get("title").and_then(|t| t.as_str()).map(String::from),
        author: info.pointer("/authors/0").and_then(|a| a.as_str()).map(String::from),
        description: info.get("description").and_then(|d| d.as_str()).map(String::from),
        cover_url: info
            .pointer("/imageLinks/thumbnail")
            .and_then(|t| t.as_str())
            .map(|u| u.replace("http://", "https://")),
        isbn: info
            .get("industryIdentifiers")
            .and_then(|ids| ids.as_array())
            .and_then(|ids| {
                ids.iter()
                    .find(|i| i.get("type").and_then(|t| t.as_str()) == Some("ISBN_13"))
                    .or_else(|| ids.first())
            })
            .and_then(|i| i.get("identifier"))
            .and_then(|i| i.as_str())
            .map(String::from),
    })
}

async fn googlebooks(state: &AppState, title: &str, author: Option<&str>) -> Option<ProviderMeta> {
    let mut q = format!("intitle:{}", crate::books::urlenc(title));
    if let Some(a) = author {
        q.push_str(&format!("+inauthor:{}", crate::books::urlenc(a)));
    }
    let url = format!("https://www.googleapis.com/books/v1/volumes?maxResults=3&q={q}");
    let v: Value = state.http.get(&url).send().await.ok()?.json().await.ok()?;
    parse_googlebooks(&v)
}

/// Best-effort lookup across enabled providers; first hit that has a
/// description wins, else the first hit at all.
pub async fn lookup(state: &AppState, title: &str, author: Option<&str>) -> Option<ProviderMeta> {
    let mut fallback: Option<ProviderMeta> = None;
    if enabled(state, "meta_googlebooks").await {
        if let Some(m) = googlebooks(state, title, author).await {
            if m.description.is_some() {
                return Some(m);
            }
            fallback = Some(m);
        }
    }
    if enabled(state, "meta_openlibrary").await {
        if let Some(m) = openlibrary(state, title, author).await {
            if m.description.is_some() {
                return Some(m);
            }
            fallback = fallback.or(Some(m));
        }
    }
    fallback
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn openlibrary_parse() {
        let v = json!({"docs":[{"title":"Dune","author_name":["Frank Herbert"],
            "cover_i": 12345, "isbn":["9780441172719"], "key":"/works/OL893415W"}]});
        let m = parse_openlibrary(&v).unwrap();
        assert_eq!(m.title.as_deref(), Some("Dune"));
        assert_eq!(m.author.as_deref(), Some("Frank Herbert"));
        assert_eq!(m.cover_url.as_deref(), Some("https://covers.openlibrary.org/b/id/12345-L.jpg"));
        assert_eq!(m.isbn.as_deref(), Some("9780441172719"));

        assert_eq!(
            parse_ol_work_description(&json!({"description":"A desert planet."})).as_deref(),
            Some("A desert planet.")
        );
        assert_eq!(
            parse_ol_work_description(&json!({"description":{"type":"/type/text","value":"Wormsign."}})).as_deref(),
            Some("Wormsign.")
        );
    }

    #[test]
    fn googlebooks_parse() {
        let v = json!({"items":[{"volumeInfo":{
            "title":"Dune","authors":["Frank Herbert"],
            "description":"Melange.","imageLinks":{"thumbnail":"http://books.google.com/x.jpg"},
            "industryIdentifiers":[{"type":"ISBN_10","identifier":"0441172717"},
                                   {"type":"ISBN_13","identifier":"9780441172719"}]}}]});
        let m = parse_googlebooks(&v).unwrap();
        assert_eq!(m.description.as_deref(), Some("Melange."));
        assert_eq!(m.isbn.as_deref(), Some("9780441172719"));
        assert!(m.cover_url.as_deref().unwrap().starts_with("https://"));
    }
}
