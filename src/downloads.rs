//! B3: download clients (qBittorrent + SABnzbd), the downloads queue, a 30s
//! monitor, and the completed-download import pipeline with remote path
//! mappings (client filesystem != our filesystem, e.g. qBit behind gluetun).

use crate::auth::{deny_non_admin, CurrentUser};
use crate::AppState;
use anyhow::{anyhow, bail, Context, Result};
use axum::{
    extract::State,
    response::{Html, IntoResponse, Redirect, Response},
    Extension, Form,
};
use maud::{html, Markup};
use serde::Deserialize;
use sqlx::Row;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct DownloadClient {
    pub id: i64,
    pub name: String,
    pub kind: String, // qbittorrent | sabnzbd
    pub host: String,
    pub port: i64,
    pub username: Option<String>,
    pub password: Option<String>, // qbit password / sab api key
    pub category: Option<String>,
    pub use_ssl: i64,
    pub enabled: i64,
    pub last_test_ok: Option<i64>,
    pub last_test_error: Option<String>,
}

impl DownloadClient {
    fn base(&self) -> String {
        let scheme = if self.use_ssl == 1 { "https" } else { "http" };
        format!("{scheme}://{}:{}", self.host, self.port)
    }
    fn protocol(&self) -> &'static str {
        if self.kind == "sabnzbd" {
            "usenet"
        } else {
            "torrent"
        }
    }
}

async fn clients(state: &AppState) -> Vec<DownloadClient> {
    sqlx::query_as::<_, DownloadClient>("SELECT * FROM download_clients ORDER BY name")
        .fetch_all(&state.pool)
        .await
        .unwrap_or_default()
}

/// Apply the client's remote→local path mappings (longest prefix wins).
pub fn map_path(mappings: &[(String, String)], path: &str) -> PathBuf {
    let mut best: Option<(&str, &str)> = None;
    for (remote, local) in mappings {
        if path.starts_with(remote.as_str())
            && best.map_or(true, |(r, _)| remote.len() > r.len())
        {
            best = Some((remote, local));
        }
    }
    match best {
        Some((remote, local)) => {
            let rest = path[remote.len()..].trim_start_matches(['/', '\\']);
            Path::new(local).join(rest.replace('/', std::path::MAIN_SEPARATOR_STR))
        }
        None => PathBuf::from(path),
    }
}

async fn mappings_for(state: &AppState, client_id: i64) -> Vec<(String, String)> {
    sqlx::query("SELECT remote_path, local_path FROM path_mappings WHERE client_id=?")
        .bind(client_id)
        .fetch_all(&state.pool)
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|r| (r.get("remote_path"), r.get("local_path")))
        .collect()
}

// ---- qBittorrent (/api/v2) ----

/// Login if credentials are set; returns the SID cookie header value (or None
/// when auth is bypassed, e.g. LAN whitelisting).
async fn qbit_login(state: &AppState, c: &DownloadClient) -> Result<Option<String>> {
    let (Some(u), Some(p)) = (&c.username, &c.password) else {
        return Ok(None);
    };
    if u.is_empty() {
        return Ok(None);
    }
    let resp = state
        .http
        .post(format!("{}/api/v2/auth/login", c.base()))
        .form(&[("username", u.as_str()), ("password", p.as_str())])
        .send()
        .await?;
    let cookie = resp
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.split(';').next())
        .map(String::from);
    let body = resp.text().await.unwrap_or_default();
    if body.trim() == "Ok." || cookie.is_some() {
        Ok(cookie)
    } else {
        bail!("qBittorrent login refused")
    }
}

async fn qbit_get(state: &AppState, c: &DownloadClient, path: &str) -> Result<String> {
    let sid = qbit_login(state, c).await?;
    let mut req = state.http.get(format!("{}{path}", c.base()));
    if let Some(s) = sid {
        req = req.header("Cookie", s);
    }
    Ok(req.send().await?.error_for_status()?.text().await?)
}

async fn qbit_add(state: &AppState, c: &DownloadClient, url: &str) -> Result<()> {
    let sid = qbit_login(state, c).await?;
    let mut form = vec![("urls", url.to_string())];
    if let Some(cat) = &c.category {
        form.push(("category", cat.clone()));
    }
    let mut req = state.http.post(format!("{}/api/v2/torrents/add", c.base()));
    if let Some(s) = sid {
        req = req.header("Cookie", s);
    }
    let body = req.form(&form).send().await?.error_for_status()?.text().await?;
    if body.contains("Fails") {
        bail!("qBittorrent rejected the torrent");
    }
    Ok(())
}

#[derive(Deserialize)]
struct QbitTorrent {
    name: String,
    hash: String,
    progress: f64,
    state: String,
    content_path: Option<String>,
    save_path: Option<String>,
}

async fn qbit_list(state: &AppState, c: &DownloadClient) -> Result<Vec<QbitTorrent>> {
    let cat = c.category.clone().unwrap_or_default();
    let body = qbit_get(state, c, &format!("/api/v2/torrents/info?category={cat}")).await?;
    Ok(serde_json::from_str(&body)?)
}

// ---- SABnzbd (?mode=) ----

fn sab_url(c: &DownloadClient, mode: &str, extra: &str) -> String {
    let key = c.password.clone().unwrap_or_default();
    format!("{}/api?mode={mode}&output=json&apikey={key}{extra}", c.base())
}

async fn sab_add(state: &AppState, c: &DownloadClient, nzb_url: &str) -> Result<String> {
    let cat = c.category.clone().unwrap_or_default();
    let u = sab_url(c, "addurl", &format!("&name={}&cat={cat}", crate::books::urlenc(nzb_url)));
    let body = state.http.get(&u).send().await?.error_for_status()?.text().await?;
    let v: serde_json::Value = serde_json::from_str(&body)?;
    if v.get("status").and_then(|s| s.as_bool()) != Some(true) {
        bail!("SABnzbd refused: {}", v.get("error").and_then(|e| e.as_str()).unwrap_or("unknown"));
    }
    v.get("nzo_ids")
        .and_then(|a| a.as_array())
        .and_then(|a| a.first())
        .and_then(|s| s.as_str())
        .map(String::from)
        .ok_or_else(|| anyhow!("SABnzbd returned no nzo_id"))
}

/// (progress 0..1, done, failed, storage_path) for one SAB job.
async fn sab_status(
    state: &AppState,
    c: &DownloadClient,
    nzo: &str,
) -> Result<(f64, bool, Option<String>, Option<String>)> {
    // still in queue?
    let q = state
        .http
        .get(sab_url(c, "queue", ""))
        .send()
        .await?
        .error_for_status()?
        .json::<serde_json::Value>()
        .await?;
    if let Some(slots) = q.pointer("/queue/slots").and_then(|s| s.as_array()) {
        if let Some(slot) = slots.iter().find(|s| s.get("nzo_id").and_then(|i| i.as_str()) == Some(nzo)) {
            let pct = slot
                .get("percentage")
                .and_then(|p| p.as_str())
                .and_then(|p| p.parse::<f64>().ok())
                .unwrap_or(0.0);
            return Ok((pct / 100.0, false, None, None));
        }
    }
    // else look in history
    let h = state
        .http
        .get(sab_url(c, "history", "&limit=100"))
        .send()
        .await?
        .error_for_status()?
        .json::<serde_json::Value>()
        .await?;
    if let Some(slots) = h.pointer("/history/slots").and_then(|s| s.as_array()) {
        if let Some(slot) = slots.iter().find(|s| s.get("nzo_id").and_then(|i| i.as_str()) == Some(nzo)) {
            let status = slot.get("status").and_then(|s| s.as_str()).unwrap_or("");
            let storage = slot.get("storage").and_then(|s| s.as_str()).map(String::from);
            return match status {
                "Completed" => Ok((1.0, true, None, storage)),
                "Failed" => Ok((1.0, true, Some("SABnzbd reported failure".into()), storage)),
                _ => Ok((0.99, false, None, storage)), // verifying/extracting
            };
        }
    }
    bail!("nzo {nzo} not found in queue or history")
}

// ---- grab ----

/// Send a release to the first enabled client for its protocol and record it.
pub async fn grab_release(state: &AppState, rel: &crate::indexer::Release) -> Result<()> {
    let client = clients(state)
        .await
        .into_iter()
        .filter(|c| c.enabled == 1)
        .find(|c| c.protocol() == rel.protocol)
        .with_context(|| format!("no enabled {} download client", rel.protocol))?;

    let external_id = match client.kind.as_str() {
        "sabnzbd" => Some(sab_add(state, &client, &rel.download_url).await?),
        _ => {
            qbit_add(state, &client, &rel.download_url).await?;
            // qBit's add returns no hash; magnets carry one, else the monitor
            // matches by name within our category.
            rel.download_url
                .split_once("btih:")
                .map(|(_, h)| h.chars().take(40).collect::<String>().to_lowercase())
        }
    };
    sqlx::query(
        "INSERT INTO downloads (client_id, external_id, title, protocol) VALUES (?,?,?,?)",
    )
    .bind(client.id)
    .bind(&external_id)
    .bind(&rel.title)
    .bind(&rel.protocol)
    .execute(&state.pool)
    .await?;
    tracing::info!("grabbed '{}' via {}", rel.title, client.name);
    Ok(())
}

// ---- 30s monitor + import ----

pub async fn monitor(state: AppState) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
    loop {
        tick.tick().await;
        if let Err(e) = poll_once(&state).await {
            tracing::warn!("download monitor: {e}");
        }
    }
}

async fn poll_once(state: &AppState) -> Result<()> {
    let rows = sqlx::query(
        "SELECT id, client_id, external_id, title, state FROM downloads
         WHERE state IN ('queued','downloading','completed')",
    )
    .fetch_all(&state.pool)
    .await?;
    if rows.is_empty() {
        return Ok(());
    }
    let all_clients = clients(state).await;
    for r in rows {
        let id: i64 = r.get("id");
        let Some(client) = all_clients.iter().find(|c| c.id == r.get::<i64, _>("client_id")) else {
            set_state(state, id, "failed", 0.0, Some("client removed"), None).await;
            continue;
        };
        let title: String = r.get("title");
        let external: Option<String> = r.get("external_id");

        let status = match client.kind.as_str() {
            "sabnzbd" => match &external {
                Some(nzo) => sab_status(state, client, nzo).await,
                None => Err(anyhow!("missing nzo_id")),
            },
            _ => qbit_status(state, client, external.as_deref(), &title).await,
        };
        match status {
            Ok((progress, done, err, path)) => {
                if let Some(e) = err {
                    set_state(state, id, "failed", progress, Some(&e), path.as_deref()).await;
                } else if done {
                    set_state(state, id, "importing", 1.0, None, path.as_deref()).await;
                    let res = import_download(state, client, id, path.as_deref()).await;
                    match res {
                        Ok(n) => {
                            set_state(state, id, "imported", 1.0, None, path.as_deref()).await;
                            tracing::info!("imported {n} file(s) from '{title}'");
                        }
                        Err(e) => {
                            set_state(state, id, "failed", 1.0, Some(&e.to_string()), path.as_deref())
                                .await
                        }
                    }
                } else {
                    set_state(state, id, "downloading", progress, None, path.as_deref()).await;
                }
            }
            Err(e) => tracing::warn!("status for '{title}': {e}"),
        }
    }
    Ok(())
}

/// qBit status: match by hash when known, else by name within our category.
async fn qbit_status(
    state: &AppState,
    c: &DownloadClient,
    hash: Option<&str>,
    title: &str,
) -> Result<(f64, bool, Option<String>, Option<String>)> {
    let list = qbit_list(state, c).await?;
    let t = list
        .iter()
        .find(|t| hash.is_some_and(|h| t.hash.eq_ignore_ascii_case(h)))
        .or_else(|| list.iter().find(|t| t.name == title))
        .ok_or_else(|| anyhow!("torrent not found in client yet"))?;
    let done = t.progress >= 1.0;
    let failed = t.state.contains("error") || t.state == "missingFiles";
    let path = t.content_path.clone().or_else(|| t.save_path.clone());
    Ok((
        t.progress,
        done,
        failed.then(|| format!("qBittorrent state: {}", t.state)),
        path,
    ))
}

async fn set_state(
    state: &AppState,
    id: i64,
    st: &str,
    progress: f64,
    error: Option<&str>,
    path: Option<&str>,
) {
    let _ = sqlx::query(
        "UPDATE downloads SET state=?, progress=?, error=?, save_path=COALESCE(?, save_path),
         updated_at=datetime('now') WHERE id=?",
    )
    .bind(st)
    .bind(progress)
    .bind(error)
    .bind(path)
    .bind(id)
    .execute(&state.pool)
    .await;
}

/// Copy book files from the (path-mapped) completed download into BOOKS_DIR,
/// then enqueue a scan so they import + enrich like any other book.
async fn import_download(
    state: &AppState,
    client: &DownloadClient,
    download_id: i64,
    remote_path: Option<&str>,
) -> Result<usize> {
    let remote = remote_path.context("client reported no path")?;
    let maps = mappings_for(state, client.id).await;
    let local = map_path(&maps, remote);
    if !local.exists() {
        bail!(
            "path not reachable: {} (add a remote path mapping for {})",
            local.display(),
            client.name
        );
    }
    let mut copied = 0;
    let mut files = vec![];
    if local.is_dir() {
        walk(&local, &mut files);
    } else {
        files.push(local.clone());
    }
    for f in files {
        let Some(ext) = f.extension().and_then(|e| e.to_str()).map(str::to_lowercase) else {
            continue;
        };
        if !["epub", "pdf", "cbz", "cbr", "mobi", "azw3", "txt"].contains(&ext.as_str()) {
            continue;
        }
        let dest = state.books_dir.join(f.file_name().context("bad filename")?);
        std::fs::copy(&f, &dest).with_context(|| format!("copying {}", f.display()))?;
        copied += 1;
    }
    if copied == 0 {
        bail!("no book files found in {}", local.display());
    }
    let _ = crate::jobs::enqueue(&state.pool, "scan", &serde_json::json!({})).await;
    tracing::info!("download {download_id}: copied {copied} book file(s)");
    Ok(copied)
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else {
            out.push(p);
        }
    }
}

// ---- connectivity test ----

pub async fn test_client(state: &AppState, c: &DownloadClient) -> Result<()> {
    match c.kind.as_str() {
        "sabnzbd" => {
            let v: serde_json::Value = state
                .http
                .get(sab_url(c, "version", ""))
                .send()
                .await
                .context("request failed")?
                .error_for_status()?
                .json()
                .await?;
            v.get("version").context("no version in response")?;
            Ok(())
        }
        _ => {
            let body = qbit_get(state, c, "/api/v2/app/version").await?;
            if body.trim_start().starts_with('v') {
                Ok(())
            } else {
                bail!("unexpected qBittorrent response")
            }
        }
    }
}

// ---- Settings → Download Clients tab ----

pub async fn clients_body(state: &AppState) -> Markup {
    let list = clients(state).await;
    let maps = sqlx::query("SELECT id, client_id, remote_path, local_path FROM path_mappings")
        .fetch_all(&state.pool)
        .await
        .unwrap_or_default();
    html! {
        h2 { "Download clients" }
        @if list.is_empty() { p .muted { "No download clients. Torrent grabs need qBittorrent; usenet (NZBGeek) grabs need SABnzbd." } }
        div .results {
            @for c in &list {
                div .result {
                    span .title { (c.name) " " span .fmt { (c.kind) } }
                    span .author { (c.host) ":" (c.port) " · category " (c.category.clone().unwrap_or_default()) }
                    @match c.last_test_ok {
                        Some(1) => span .status-badge .completed { "OK" },
                        Some(_) => span .status-badge .failed title=[c.last_test_error.as_deref()] { "Failed" },
                        None => span .status-badge .queued { "Untested" },
                    }
                    form .inline hx-post="/settings/clients/test" hx-target="this" hx-swap="outerHTML" {
                        input type="hidden" name="id" value=(c.id);
                        button .btn .btn-sm type="submit" { "Test" }
                    }
                    form .inline hx-post="/settings/clients/delete" hx-swap="none" hx-confirm={ "Remove client " (c.name) "?" } {
                        input type="hidden" name="id" value=(c.id);
                        button .link { "remove" }
                    }
                }
            }
        }
        h2 { "Add client" }
        form .editform action="/settings/clients/add" method="post" {
            label { "Name" } input type="text" name="name" required;
            label { "Kind" }
            select name="kind" {
                option value="qbittorrent" { "qBittorrent (torrent)" }
                option value="sabnzbd" { "SABnzbd (usenet)" }
            }
            label { "Host" } input type="text" name="host" placeholder="192.168.68.246" required;
            label { "Port" } input type="number" name="port" value="8080" required;
            label { "Username" } input type="text" name="username";
            label { "Password / API key" } input type="text" name="password";
            label { "Category" } input type="text" name="category" value="shelfarrs";
            label .check { input type="checkbox" name="use_ssl" value="1"; " Use SSL" }
            button .btn type="submit" { "Add client" }
        }
        h2 { "Remote path mappings" }
        p .muted { "When a client saves to a path this app can't see (containers, other hosts), map the client's path to a reachable one." }
        div .results {
            @for m in &maps {
                @let cid: i64 = m.get("client_id");
                div .result {
                    span .title { code { (m.get::<String, _>("remote_path")) } " → " code { (m.get::<String, _>("local_path")) } }
                    span .author { "client #" (cid) }
                    form .inline hx-post="/settings/clients/mappings/delete" hx-swap="none" {
                        input type="hidden" name="id" value=(m.get::<i64, _>("id"));
                        button .link { "remove" }
                    }
                }
            }
        }
        form .editform action="/settings/clients/mappings/add" method="post" {
            label { "Client" }
            select name="client_id" {
                @for c in &list { option value=(c.id) { (c.name) } }
            }
            label { "Remote path" } input type="text" name="remote_path" placeholder="/media/tank/Downloads" required;
            label { "Local path" } input type="text" name="local_path" placeholder="\\\\beelink\\Downloads or /mnt/tank/Downloads" required;
            button .btn type="submit" { "Add mapping" }
        }
    }
}

// ---- handlers ----

#[derive(Deserialize)]
pub struct AddClientForm {
    pub name: String,
    pub kind: String,
    pub host: String,
    pub port: i64,
    pub username: Option<String>,
    pub password: Option<String>,
    pub category: Option<String>,
    pub use_ssl: Option<String>,
}

pub async fn add(
    State(state): State<AppState>,
    Extension(me): Extension<CurrentUser>,
    Form(f): Form<AddClientForm>,
) -> Response {
    if let Some(r) = deny_non_admin(&me) {
        return r;
    }
    let kind = if f.kind == "sabnzbd" { "sabnzbd" } else { "qbittorrent" };
    let blank = |s: Option<String>| s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let _ = sqlx::query(
        "INSERT INTO download_clients (name, kind, host, port, username, password, category, use_ssl)
         VALUES (?,?,?,?,?,?,?,?)",
    )
    .bind(f.name.trim())
    .bind(kind)
    .bind(f.host.trim())
    .bind(f.port)
    .bind(blank(f.username))
    .bind(blank(f.password))
    .bind(blank(f.category).or_else(|| Some("shelfarrs".into())))
    .bind(if f.use_ssl.is_some() { 1i64 } else { 0 })
    .execute(&state.pool)
    .await;
    Redirect::to("/settings?tab=clients").into_response()
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
    let _ = sqlx::query("DELETE FROM download_clients WHERE id=?").bind(f.id).execute(&state.pool).await;
    let _ = sqlx::query("DELETE FROM path_mappings WHERE client_id=?").bind(f.id).execute(&state.pool).await;
    (axum::http::StatusCode::OK, "removed").into_response()
}

pub async fn test_handler(
    State(state): State<AppState>,
    Extension(me): Extension<CurrentUser>,
    Form(f): Form<IdForm>,
) -> Response {
    if let Some(r) = deny_non_admin(&me) {
        return r;
    }
    let Some(c) = sqlx::query_as::<_, DownloadClient>("SELECT * FROM download_clients WHERE id=?")
        .bind(f.id)
        .fetch_optional(&state.pool)
        .await
        .ok()
        .flatten()
    else {
        return Html("<span class=\"err\">not found</span>".to_string()).into_response();
    };
    let (ok, err) = match test_client(&state, &c).await {
        Ok(_) => (1i64, None),
        Err(e) => (0i64, Some(e.to_string())),
    };
    let _ = sqlx::query("UPDATE download_clients SET last_test_ok=?, last_test_error=? WHERE id=?")
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
pub struct MappingForm {
    pub client_id: i64,
    pub remote_path: String,
    pub local_path: String,
}

pub async fn mapping_add(
    State(state): State<AppState>,
    Extension(me): Extension<CurrentUser>,
    Form(f): Form<MappingForm>,
) -> Response {
    if let Some(r) = deny_non_admin(&me) {
        return r;
    }
    let _ = sqlx::query("INSERT INTO path_mappings (client_id, remote_path, local_path) VALUES (?,?,?)")
        .bind(f.client_id)
        .bind(f.remote_path.trim())
        .bind(f.local_path.trim())
        .execute(&state.pool)
        .await;
    Redirect::to("/settings?tab=clients").into_response()
}

pub async fn mapping_delete(
    State(state): State<AppState>,
    Extension(me): Extension<CurrentUser>,
    Form(f): Form<IdForm>,
) -> Response {
    if let Some(r) = deny_non_admin(&me) {
        return r;
    }
    let _ = sqlx::query("DELETE FROM path_mappings WHERE id=?").bind(f.id).execute(&state.pool).await;
    (axum::http::StatusCode::OK, "removed").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_longest_prefix() {
        let maps = vec![
            ("/media/tank".into(), r"Z:\tank".into()),
            ("/media/tank/Downloads".into(), r"Z:\dl".into()),
        ];
        assert_eq!(
            map_path(&maps, "/media/tank/Downloads/books/x.epub"),
            Path::new(r"Z:\dl").join("books").join("x.epub")
        );
        assert_eq!(
            map_path(&maps, "/media/tank/other/y.epub"),
            Path::new(r"Z:\tank").join("other").join("y.epub")
        );
        // unmapped passes through
        assert_eq!(map_path(&[], "/data/z.epub"), PathBuf::from("/data/z.epub"));
    }
}
