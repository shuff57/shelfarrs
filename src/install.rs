//! The plugin ecosystem manager: repository indexes, integrity-checked install
//! (verify sha256 -> SafeExtract -> atomic move -> hot reload), and uninstall.
//! Zero downtime — a WASM source re-instantiates live, a viewer serves next open.

use crate::plugin::{scan_installed, PluginManifest};
use crate::{page, AppState};
use anyhow::{anyhow, bail, Context, Result};
use axum::{
    extract::State,
    response::{Html, IntoResponse, Redirect, Response},
    Form,
};
use maud::html;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::io::Read;
use std::path::Path;

const REPOS_KEY: &str = "plugin_repositories";

#[derive(Debug, Deserialize)]
pub struct RepoIndex {
    #[serde(default)]
    #[allow(dead_code)] // informational field in the index format
    pub name: String,
    pub plugins: Vec<RepoEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct RepoEntry {
    pub id: String,
    pub kind: String,
    pub version: String,
    pub package: String, // zip URL
    pub sha256: String,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub author: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

// ---- settings-backed repo list (JSON in the settings table, not its own table) ----
// ponytail: repos = JSON array in settings. A table only if repo metadata grows.

async fn repo_urls(state: &AppState) -> Vec<String> {
    let raw: Option<String> = sqlx::query("SELECT value FROM settings WHERE key=?")
        .bind(REPOS_KEY)
        .fetch_optional(&state.pool)
        .await
        .ok()
        .flatten()
        .map(|r| r.get("value"));
    raw.and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
        .unwrap_or_default()
}

async fn set_repo_urls(state: &AppState, urls: &[String]) -> Result<()> {
    let json = serde_json::to_string(urls)?;
    sqlx::query(
        "INSERT INTO settings (key, value) VALUES (?, ?)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value",
    )
    .bind(REPOS_KEY)
    .bind(json)
    .execute(&state.pool)
    .await?;
    Ok(())
}

async fn fetch_index(state: &AppState, url: &str) -> Result<RepoIndex> {
    let body = state
        .http
        .get(url)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    Ok(serde_json::from_slice(&body).with_context(|| format!("parsing index {url}"))?)
}

// ---- integrity-checked, sandbox-safe install ----

/// Unpack a plugin zip into `dest` with a zip-slip guard and size caps. Pure over
/// the filesystem so the zip-slip case is unit-tested below.
pub fn safe_extract(bytes: &[u8], dest: &Path) -> Result<()> {
    const MAX_TOTAL: u64 = 200 * 1024 * 1024; // 200 MB unpacked
    const MAX_FILES: usize = 5000;
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
    if zip.len() > MAX_FILES {
        bail!("zip has too many entries ({})", zip.len());
    }
    std::fs::create_dir_all(dest)?;
    let mut total: u64 = 0;
    for i in 0..zip.len() {
        let mut f = zip.by_index(i)?;
        // enclosed_name() returns None for anything that escapes (../, absolute, ...).
        let rel = f
            .enclosed_name()
            .ok_or_else(|| anyhow!("unsafe path in zip: {}", f.name()))?;
        let out = dest.join(&rel);
        if !out.starts_with(dest) {
            bail!("zip slip blocked: {}", rel.display());
        }
        if f.is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(p) = out.parent() {
            std::fs::create_dir_all(p)?;
        }
        total = total.saturating_add(f.size());
        if total > MAX_TOTAL {
            bail!("zip exceeds unpacked size cap");
        }
        let mut o = std::fs::File::create(&out)?;
        std::io::copy(&mut f.by_ref().take(MAX_TOTAL), &mut o)?;
    }
    Ok(())
}

/// Full install: fetch -> verify sha256 -> extract to temp -> atomic swap -> reload.
pub async fn install_entry(state: &AppState, entry: &RepoEntry) -> Result<()> {
    let bytes = state
        .http
        .get(&entry.package)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;

    let got = hex(&Sha256::digest(&bytes));
    if got != entry.sha256.to_lowercase() {
        bail!("sha256 mismatch for {}: expected {}, got {got}", entry.id, entry.sha256);
    }

    let final_dir = state.plugins_dir.join(&entry.id);
    let tmp_dir = state.plugins_dir.join(format!(".tmp-{}", entry.id));
    let _ = std::fs::remove_dir_all(&tmp_dir);
    safe_extract(&bytes, &tmp_dir)?;

    // The manifest must exist and match the claimed id (defence-in-depth).
    let mf: PluginManifest = serde_json::from_str(
        &std::fs::read_to_string(tmp_dir.join("plugin.json"))
            .context("package missing plugin.json")?,
    )?;
    if mf.id != entry.id {
        let _ = std::fs::remove_dir_all(&tmp_dir);
        bail!("package id {} != index id {}", mf.id, entry.id);
    }

    // Atomic-ish swap on the same filesystem.
    let _ = std::fs::remove_dir_all(&final_dir);
    std::fs::rename(&tmp_dir, &final_dir)
        .with_context(|| format!("installing {}", entry.id))?;

    // Hot reload: a source re-instantiates now; a viewer just serves next open.
    state.reload_sources();
    tracing::info!("installed plugin {} v{} (hot)", entry.id, entry.version);
    Ok(())
}

pub fn uninstall(state: &AppState, id: &str) -> Result<()> {
    let dir = state.plugins_dir.join(id);
    if !dir.starts_with(&state.plugins_dir) {
        bail!("refusing to delete outside plugins dir");
    }
    std::fs::remove_dir_all(&dir).with_context(|| format!("removing {id}"))?;
    // Drop this plugin's kv namespace.
    state.kv.lock().unwrap().retain(|(pid, _), _| pid != id);
    state.reload_sources();
    tracing::info!("uninstalled plugin {id} (hot)");
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

// ---- Settings -> Plugins UI ----

pub async fn plugins_page(State(state): State<AppState>) -> Html<String> {
    let installed: Vec<PluginManifest> =
        scan_installed(&state.plugins_dir).into_iter().map(|i| i.manifest).collect();
    let installed_ver: std::collections::HashMap<&str, &str> =
        installed.iter().map(|m| (m.id.as_str(), m.version.as_str())).collect();

    let repos = repo_urls(&state).await;
    let mut available: Vec<RepoEntry> = vec![];
    for url in &repos {
        match fetch_index(&state, url).await {
            Ok(idx) => available.extend(idx.plugins),
            Err(e) => tracing::warn!("index {url} failed: {e}"),
        }
    }

    let body = html! {
        h1 { "Plugins" }

        section .repos {
            h2 { "Repositories" }
            @if repos.is_empty() { p .muted { "No repositories yet. Add a repo index URL below." } }
            ul {
                @for u in &repos {
                    li {
                        code { (u) }
                        form .inline hx-post="/settings/plugins/repos/remove" hx-swap="none" {
                            input type="hidden" name="url" value=(u);
                            button .link { "remove" }
                        }
                    }
                }
            }
            form .search action="/settings/plugins/repos/add" method="post" {
                input type="url" name="url" placeholder="https://…/index.json" required;
                button .btn type="submit" { "Add repository" }
            }
        }

        section {
            h2 { "Available" }
            @let fresh: Vec<&RepoEntry> = available.iter()
                .filter(|e| installed_ver.get(e.id.as_str()).map_or(true, |v| *v != e.version)).collect();
            @if fresh.is_empty() { p .muted { "Nothing new to install." } }
            div .results {
                @for e in &fresh {
                    div .result {
                        span .title { (e.id) " " span .fmt { (e.kind) } }
                        span .author {
                            "v" (e.version)
                            @if let Some(a)=&e.author { " · " (a) }
                            @if let Some(d)=&e.description { " — " (d) }
                        }
                        span .caps { "grants: " @if e.capabilities.is_empty() { "none" } @else { (e.capabilities.join(", ")) } }
                        form hx-post="/settings/plugins/install" hx-swap="outerHTML" hx-target="this" .addform {
                            input type="hidden" name="id" value=(e.id);
                            button .btn type="submit" {
                                @if installed_ver.contains_key(e.id.as_str()) { "Update" } @else { "Install" }
                            }
                        }
                    }
                }
            }
        }

        section {
            h2 { "Installed" }
            @if installed.is_empty() { p .muted { "No plugins installed." } }
            div .results {
                @for m in &installed {
                    div .result {
                        span .title { (m.name) " " span .fmt { (m.kind) } }
                        span .author {
                            "v" (m.version)
                            @if let Some(d)=&m.description { " — " (d) }
                        }
                        @if !m.capabilities.is_empty() { span .caps { "grants: " (m.capabilities.join(", ")) } }
                        form hx-post="/settings/plugins/uninstall" hx-swap="outerHTML" hx-target="this" .addform {
                            input type="hidden" name="id" value=(m.id);
                            button .btn .danger type="submit" { "Uninstall" }
                        }
                    }
                }
            }
        }
    };
    Html(page("Plugins", body).into_string())
}

#[derive(Deserialize)]
pub struct IdForm {
    pub id: String,
}
#[derive(Deserialize)]
pub struct UrlForm {
    pub url: String,
}

pub async fn install(State(state): State<AppState>, Form(f): Form<IdForm>) -> Html<String> {
    // Find the entry across configured repos.
    let repos = repo_urls(&state).await;
    let mut found = None;
    for url in &repos {
        if let Ok(idx) = fetch_index(&state, url).await {
            if let Some(e) = idx.plugins.into_iter().find(|e| e.id == f.id) {
                found = Some(e);
                break;
            }
        }
    }
    let Some(entry) = found else {
        return Html("<span class=\"err\">Not found in any repository</span>".into());
    };
    match install_entry(&state, &entry).await {
        Ok(_) => Html(format!("<span class=\"queued\">Installed {} ✓</span>", entry.id)),
        Err(e) => Html(format!("<span class=\"err\">Install failed: {e}</span>")),
    }
}

pub async fn uninstall_handler(State(state): State<AppState>, Form(f): Form<IdForm>) -> Html<String> {
    match uninstall(&state, &f.id) {
        Ok(_) => Html(format!("<span class=\"queued\">Uninstalled {} ✓</span>", f.id)),
        Err(e) => Html(format!("<span class=\"err\">Uninstall failed: {e}</span>")),
    }
}

pub async fn repos_add(State(state): State<AppState>, Form(f): Form<UrlForm>) -> Response {
    let mut urls = repo_urls(&state).await;
    if !urls.contains(&f.url) {
        urls.push(f.url);
        let _ = set_repo_urls(&state, &urls).await;
    }
    Redirect::to("/settings/plugins").into_response()
}

pub async fn repos_remove(State(state): State<AppState>, Form(f): Form<UrlForm>) -> Response {
    let urls: Vec<String> = repo_urls(&state).await.into_iter().filter(|u| u != &f.url).collect();
    let _ = set_repo_urls(&state, &urls).await;
    (axum::http::StatusCode::OK, "removed").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn zip_with(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default();
            for (name, data) in entries {
                w.start_file(*name, opts).unwrap();
                w.write_all(data).unwrap();
            }
            w.finish().unwrap();
        }
        buf.into_inner()
    }

    #[test]
    fn extracts_safely() {
        let dir = std::env::temp_dir().join(format!("se-ok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let z = zip_with(&[("plugin.json", b"{}"), ("ui/x.js", b"hi")]);
        safe_extract(&z, &dir).unwrap();
        assert!(dir.join("plugin.json").is_file());
        assert!(dir.join("ui/x.js").is_file());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn blocks_zip_slip() {
        let dir = std::env::temp_dir().join(format!("se-evil-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let z = zip_with(&[("../evil.txt", b"pwned")]);
        let r = safe_extract(&z, &dir);
        assert!(r.is_err(), "zip-slip must be rejected");
        // and nothing was written to the parent
        assert!(!dir.parent().unwrap().join("evil.txt").exists());
        std::fs::remove_dir_all(&dir).ok();
    }
}
