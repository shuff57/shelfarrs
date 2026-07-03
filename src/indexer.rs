//! B2: torznab/newznab indexers. One client serves both protocols — the API
//! shape is shared (`GET {url}/api?t=search&q=…&apikey=…&cat=…`), responses are
//! RSS XML with torznab attributes. Parsed with the same hand-rolled tag scan
//! as meta.rs; torznab feeds are machine-generated.

use crate::auth::{deny_non_admin, CurrentUser};
use crate::meta::{all_tags, attr, tag_text, unescape};
use crate::AppState;
use anyhow::{Context, Result};
use axum::{
    extract::State,
    response::{Html, IntoResponse, Redirect, Response},
    Extension, Form,
};
use maud::{html, Markup};
use serde::Deserialize;
use sqlx::Row;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Indexer {
    pub id: i64,
    pub name: String,
    pub protocol: String, // torrent | usenet
    pub url: String,
    pub api_key: Option<String>,
    pub categories: Option<String>,
    pub priority: i64,
    pub enabled: i64,
    pub last_test_ok: Option<i64>,
    pub last_test_error: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Release {
    pub title: String,
    pub indexer_id: i64,
    pub indexer: String,
    pub protocol: String,
    pub size: Option<i64>,
    pub seeders: Option<i64>,
    pub grabs: Option<i64>,
    pub download_url: String,
    pub published: Option<String>,
}

pub async fn all(state: &AppState) -> Vec<Indexer> {
    sqlx::query_as::<_, Indexer>("SELECT * FROM indexers ORDER BY priority, name")
        .fetch_all(&state.pool)
        .await
        .unwrap_or_default()
}

// ---- search ----

fn search_url(ix: &Indexer, query: &str) -> String {
    let mut u = format!(
        "{}/api?t=search&q={}&limit=100&extended=1",
        ix.url.trim_end_matches('/'),
        crate::books::urlenc(query)
    );
    if let Some(k) = &ix.api_key {
        u.push_str(&format!("&apikey={k}"));
    }
    if let Some(c) = &ix.categories {
        if !c.trim().is_empty() {
            u.push_str(&format!("&cat={c}"));
        }
    }
    u
}

/// Parse a torznab/newznab RSS response into releases.
fn parse_rss(xml: &str, ix: &Indexer) -> Vec<Release> {
    let mut out = vec![];
    // split on <item> blocks; each ends at </item>
    let mut at = 0;
    while let Some(i) = xml[at..].find("<item") {
        let start = at + i;
        let end = match xml[start..].find("</item>") {
            Some(e) => start + e,
            None => break,
        };
        let item = &xml[start..end];
        at = end + 7;

        let title = tag_text(item, "title").map(|t| unescape(&t)).unwrap_or_default();
        if title.is_empty() {
            continue;
        }
        // torznab attrs: <torznab:attr name="seeders" value="12"/>
        let mut seeders = None;
        let mut grabs = None;
        let mut size_attr = None;
        for t in all_tags(item, "torznab:attr").iter().chain(all_tags(item, "newznab:attr").iter()) {
            match attr(t, "name").as_deref() {
                Some("seeders") => seeders = attr(t, "value").and_then(|v| v.parse().ok()),
                Some("grabs") => grabs = attr(t, "value").and_then(|v| v.parse().ok()),
                Some("size") => size_attr = attr(t, "value").and_then(|v| v.parse().ok()),
                _ => {}
            }
        }
        let enclosure = all_tags(item, "enclosure").into_iter().next();
        let size = size_attr
            .or_else(|| tag_text(item, "size").and_then(|s| s.parse().ok()))
            .or_else(|| enclosure.as_ref().and_then(|e| attr(e, "length")).and_then(|l| l.parse().ok()))
            .filter(|s: &i64| *s > 0);
        let download_url = enclosure
            .as_ref()
            .and_then(|e| attr(e, "url"))
            .or_else(|| tag_text(item, "link"))
            .map(|u| unescape(&u))
            .unwrap_or_default();
        if download_url.is_empty() {
            continue;
        }
        out.push(Release {
            title,
            indexer_id: ix.id,
            indexer: ix.name.clone(),
            protocol: ix.protocol.clone(),
            size,
            seeders,
            grabs,
            download_url,
            published: tag_text(item, "pubDate"),
        });
    }
    out
}

/// Fan out a query to every enabled indexer; merge, sort by seeders desc
/// (usenet releases have none — they sort after unless newer logic in B4).
pub async fn search_all(state: &AppState, query: &str) -> Vec<Release> {
    let indexers: Vec<Indexer> = all(state).await.into_iter().filter(|i| i.enabled == 1).collect();
    let mut tasks = vec![];
    for ix in indexers {
        let http = state.http.clone();
        let q = query.to_string();
        tasks.push(tokio::spawn(async move {
            let url = search_url(&ix, &q);
            match http.get(&url).send().await.and_then(|r| r.error_for_status()) {
                Ok(resp) => match resp.text().await {
                    Ok(body) => parse_rss(&body, &ix),
                    Err(e) => {
                        tracing::warn!("indexer {} body failed: {e}", ix.name);
                        vec![]
                    }
                },
                Err(e) => {
                    tracing::warn!("indexer {} search failed: {e}", ix.name);
                    vec![]
                }
            }
        }));
    }
    let mut releases = vec![];
    for t in tasks {
        if let Ok(mut r) = t.await {
            releases.append(&mut r);
        }
    }
    releases.sort_by_key(|r| std::cmp::Reverse(r.seeders.unwrap_or(0)));
    releases
}

/// Capabilities ping — the standard indexer connectivity test.
pub async fn test_indexer(state: &AppState, ix: &Indexer) -> Result<()> {
    let mut u = format!("{}/api?t=caps", ix.url.trim_end_matches('/'));
    if let Some(k) = &ix.api_key {
        u.push_str(&format!("&apikey={k}"));
    }
    let body = state
        .http
        .get(&u)
        .send()
        .await
        .context("request failed")?
        .error_for_status()
        .context("http error")?
        .text()
        .await?;
    if body.contains("<caps") || body.contains("<rss") {
        Ok(())
    } else if let Some(desc) = tag_text(&body, "error").or_else(|| {
        all_tags(&body, "error").first().and_then(|t| attr(t, "description"))
    }) {
        anyhow::bail!("{desc}")
    } else {
        anyhow::bail!("unexpected response (not torznab/newznab)")
    }
}

// ---- Settings → Indexers tab ----

pub async fn indexers_body(state: &AppState, preset: Option<&str>) -> Markup {
    let indexers = all(state).await;
    // NZBGeek preset just prefills the add form.
    let (p_name, p_url, p_proto) = match preset {
        Some("nzbgeek") => ("NZBGeek", "https://api.nzbgeek.info", "usenet"),
        _ => ("", "", "torrent"),
    };
    html! {
        h2 { "Indexers" }
        @if indexers.is_empty() { p .muted { "No indexers yet. Add a torznab (torrent) or newznab (usenet, e.g. NZBGeek) indexer below." } }
        div .results {
            @for ix in &indexers {
                div .result {
                    span .title { (ix.name) " " span .fmt { (ix.protocol) } }
                    span .author { (ix.url) " · priority " (ix.priority) }
                    @match ix.last_test_ok {
                        Some(1) => span .status-badge .completed { "OK" },
                        Some(_) => span .status-badge .failed title=[ix.last_test_error.as_deref()] { "Failed" },
                        None => span .status-badge .queued { "Untested" },
                    }
                    form .inline hx-post="/settings/indexers/test" hx-target="this" hx-swap="outerHTML" {
                        input type="hidden" name="id" value=(ix.id);
                        button .btn .btn-sm type="submit" { "Test" }
                    }
                    form .inline hx-post="/settings/indexers/toggle" hx-swap="none" {
                        input type="hidden" name="id" value=(ix.id);
                        button .link { @if ix.enabled == 1 { "disable" } @else { "enable" } }
                    }
                    form .inline hx-post="/settings/indexers/delete" hx-swap="none" hx-confirm={ "Remove indexer " (ix.name) "?" } {
                        input type="hidden" name="id" value=(ix.id);
                        button .link { "remove" }
                    }
                }
            }
        }
        h2 { "Add indexer" }
        p .muted {
            "Presets: "
            a href="/settings?tab=indexers&preset=nzbgeek" { "NZBGeek" }
            " — or fill in any torznab/newznab endpoint (Prowlarr: copy the app's torznab URL)."
        }
        form .editform action="/settings/indexers/add" method="post" {
            label { "Name" } input type="text" name="name" value=(p_name) required;
            label { "URL" } input type="text" name="url" value=(p_url) placeholder="https://…" required;
            label { "API key" } input type="text" name="api_key";
            label { "Protocol" }
            select name="protocol" {
                option value="torrent" selected[p_proto == "torrent"] { "Torrent (torznab)" }
                option value="usenet" selected[p_proto == "usenet"] { "Usenet (newznab)" }
            }
            label { "Categories" } input type="text" name="categories" value="7000,7020";
            label { "Priority" } input type="number" name="priority" value="25";
            button .btn type="submit" { "Add indexer" }
        }
    }
}

// ---- handlers ----

#[derive(Deserialize)]
pub struct AddForm {
    pub name: String,
    pub url: String,
    pub api_key: Option<String>,
    pub protocol: String,
    pub categories: Option<String>,
    pub priority: Option<i64>,
}

pub async fn add(
    State(state): State<AppState>,
    Extension(me): Extension<CurrentUser>,
    Form(f): Form<AddForm>,
) -> Response {
    if let Some(r) = deny_non_admin(&me) {
        return r;
    }
    let proto = if f.protocol == "usenet" { "usenet" } else { "torrent" };
    let _ = sqlx::query(
        "INSERT INTO indexers (name, protocol, url, api_key, categories, priority) VALUES (?,?,?,?,?,?)",
    )
    .bind(f.name.trim())
    .bind(proto)
    // tolerate pasting the full ".../api" endpoint — we append /api ourselves
    .bind(f.url.trim().trim_end_matches('/').trim_end_matches("/api"))
    .bind(f.api_key.map(|k| k.trim().to_string()).filter(|k| !k.is_empty()))
    .bind(f.categories.map(|c| c.trim().to_string()).filter(|c| !c.is_empty()))
    .bind(f.priority.unwrap_or(25))
    .execute(&state.pool)
    .await;
    Redirect::to("/settings?tab=indexers").into_response()
}

#[derive(Deserialize)]
pub struct IdForm {
    pub id: i64,
}

pub async fn delete(
    State(state): State<AppState>,
    Extension(me): Extension<CurrentUser>,
    Form(f): Form<IdForm>,
) -> Response {
    if let Some(r) = deny_non_admin(&me) {
        return r;
    }
    let _ = sqlx::query("DELETE FROM indexers WHERE id=?").bind(f.id).execute(&state.pool).await;
    (axum::http::StatusCode::OK, "removed").into_response()
}

pub async fn toggle(
    State(state): State<AppState>,
    Extension(me): Extension<CurrentUser>,
    Form(f): Form<IdForm>,
) -> Response {
    if let Some(r) = deny_non_admin(&me) {
        return r;
    }
    let _ = sqlx::query("UPDATE indexers SET enabled = 1 - enabled WHERE id=?")
        .bind(f.id)
        .execute(&state.pool)
        .await;
    (axum::http::StatusCode::OK, "toggled").into_response()
}

pub async fn test_handler(
    State(state): State<AppState>,
    Extension(me): Extension<CurrentUser>,
    Form(f): Form<IdForm>,
) -> Response {
    if let Some(r) = deny_non_admin(&me) {
        return r;
    }
    let Some(ix) = sqlx::query_as::<_, Indexer>("SELECT * FROM indexers WHERE id=?")
        .bind(f.id)
        .fetch_optional(&state.pool)
        .await
        .ok()
        .flatten()
    else {
        return Html("<span class=\"err\">not found</span>".to_string()).into_response();
    };
    let (ok, err) = match test_indexer(&state, &ix).await {
        Ok(_) => (1i64, None),
        Err(e) => (0i64, Some(e.to_string())),
    };
    let _ = sqlx::query("UPDATE indexers SET last_test_ok=?, last_test_error=? WHERE id=?")
        .bind(ok)
        .bind(&err)
        .bind(f.id)
        .execute(&state.pool)
        .await;
    let m = match err {
        None => html! { span .status-badge .completed { "OK ✓" } },
        Some(e) => html! { span .status-badge .failed title=(e) { "Failed" } },
    };
    Html(m.into_string()).into_response()
}

#[derive(Deserialize)]
pub struct GrabForm {
    pub payload: String,
}

/// Grab a release: hand it to the matching download client (B3).
pub async fn grab(
    State(state): State<AppState>,
    Extension(me): Extension<CurrentUser>,
    Form(f): Form<GrabForm>,
) -> Response {
    if let Some(r) = deny_non_admin(&me) {
        return r;
    }
    let Ok(rel) = serde_json::from_str::<Release>(&f.payload) else {
        return Html("<span class=\"err\">bad release payload</span>".to_string()).into_response();
    };
    match crate::downloads::grab_release(&state, &rel).await {
        Ok(_) => Html("<span class=\"queued\">Grabbed ✓</span>".to_string()).into_response(),
        Err(e) => Html(format!("<span class=\"err\">{e}</span>")).into_response(),
    }
}

/// Human-readable size for release tables.
pub fn fmt_size(bytes: i64) -> String {
    const G: f64 = 1_073_741_824.0;
    const M: f64 = 1_048_576.0;
    let b = bytes as f64;
    if b >= G {
        format!("{:.1} GB", b / G)
    } else {
        format!("{:.0} MB", b / M)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ix() -> Indexer {
        Indexer {
            id: 1,
            name: "Test".into(),
            protocol: "torrent".into(),
            url: "https://x".into(),
            api_key: Some("k".into()),
            categories: Some("7000,7020".into()),
            priority: 25,
            enabled: 1,
            last_test_ok: None,
            last_test_error: None,
        }
    }

    #[test]
    fn builds_search_url() {
        let u = search_url(&ix(), "the hobbit");
        assert!(u.starts_with("https://x/api?t=search&q=the%20hobbit"));
        assert!(u.contains("&apikey=k") && u.contains("&cat=7000,7020"));
    }

    #[test]
    fn parses_torznab_rss() {
        let xml = r#"<rss><channel>
          <item>
            <title>The Hobbit (1937) EPUB &amp; MOBI</title>
            <link>https://x/details/1</link>
            <pubDate>Tue, 01 Jul 2026 10:00:00 +0000</pubDate>
            <enclosure url="https://x/dl/1.torrent" length="2097152" type="application/x-bittorrent"/>
            <torznab:attr name="seeders" value="42"/>
            <torznab:attr name="grabs" value="7"/>
          </item>
          <item><title></title></item>
        </channel></rss>"#;
        let rel = parse_rss(xml, &ix());
        assert_eq!(rel.len(), 1);
        let r = &rel[0];
        assert_eq!(r.title, "The Hobbit (1937) EPUB & MOBI");
        assert_eq!(r.seeders, Some(42));
        assert_eq!(r.size, Some(2_097_152));
        assert_eq!(r.download_url, "https://x/dl/1.torrent");
    }

    #[test]
    fn size_formatting() {
        assert_eq!(fmt_size(2_097_152), "2 MB");
        assert_eq!(fmt_size(1_610_612_736), "1.5 GB");
    }
}
