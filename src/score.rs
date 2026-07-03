//! B4: release scoring against a quality profile. Base 100 with hard rejects
//! (forbidden/missing words, size range, seeders) and bonuses (format ladder,
//! preferred words, seeders). Usenet releases are exempt from seeder/size
//! rejects when those attributes are absent — newznab feeds often omit them.

use crate::indexer::Release;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Profile {
    pub id: i64,
    pub name: String,
    pub formats: String,
    pub cutoff: String,
    pub min_size_mb: Option<f64>,
    pub max_size_mb: Option<f64>,
    pub preferred_words: Option<String>,
    pub must_contain: Option<String>,
    pub must_not_contain: Option<String>,
    pub min_seeders: i64,
    pub is_default: i64,
}

/// Higher = better ebook format. Unknown formats rank 0.
pub fn format_rank(fmt: &str) -> i64 {
    match fmt {
        "epub" => 100,
        "azw3" => 90,
        "mobi" => 80,
        "pdf" => 60,
        "cbz" | "cbr" => 50,
        "txt" => 30,
        _ => 0,
    }
}

/// First allowed format detected in a release title (word match, lowercased).
pub fn detect_format(title: &str) -> Option<&'static str> {
    let t = title.to_lowercase();
    ["epub", "azw3", "mobi", "pdf", "cbz", "cbr", "txt"]
        .into_iter()
        .find(|f| t.contains(f))
}

#[derive(Debug, PartialEq)]
pub struct Scored {
    pub total: i64,
    pub rejected: Option<String>,
}

fn words(list: &Option<String>) -> Vec<String> {
    list.as_deref()
        .unwrap_or("")
        .split(',')
        .map(|w| w.trim().to_lowercase())
        .filter(|w| !w.is_empty())
        .collect()
}

pub fn score(rel: &Release, p: &Profile) -> Scored {
    let title = rel.title.to_lowercase();
    let reject = |why: &str| Scored { total: -1, rejected: Some(why.into()) };

    for w in words(&p.must_not_contain) {
        if title.contains(&w) {
            return reject(&format!("contains forbidden word '{w}'"));
        }
    }
    for w in words(&p.must_contain) {
        if !title.contains(&w) {
            return reject(&format!("missing required word '{w}'"));
        }
    }
    // size range (skip when the feed omitted size)
    if let Some(bytes) = rel.size {
        let mb = bytes as f64 / 1_048_576.0;
        if let Some(min) = p.min_size_mb {
            if mb < min {
                return reject("below minimum size");
            }
        }
        if let Some(max) = p.max_size_mb {
            if mb > max {
                return reject("above maximum size");
            }
        }
    }
    // seeders gate torrents only; usenet has none
    if rel.protocol == "torrent" && rel.seeders.unwrap_or(0) < p.min_seeders {
        return reject("not enough seeders");
    }

    let allowed: Vec<String> = words(&Some(p.formats.clone()));
    let mut total = 100i64;
    match detect_format(&rel.title) {
        Some(f) if allowed.iter().any(|a| a == f) => total += format_rank(f) / 5,
        Some(f) => return reject(&format!("format '{f}' not allowed by profile")),
        None => total -= 8, // format unstated (common on usenet) — mild penalty
    }
    for w in words(&p.preferred_words) {
        if title.contains(&w) {
            total += 5;
        }
    }
    total += rel.seeders.unwrap_or(0).min(10);
    Scored { total, rejected: None }
}

/// The single best non-rejected release, highest score first.
pub fn pick_best<'a>(releases: &'a [Release], p: &Profile) -> Option<(&'a Release, i64)> {
    releases
        .iter()
        .filter_map(|r| {
            let s = score(r, p);
            s.rejected.is_none().then_some((r, s.total))
        })
        .max_by_key(|(_, s)| *s)
}

pub async fn default_profile(state: &crate::AppState) -> Profile {
    sqlx::query_as::<_, Profile>(
        "SELECT * FROM quality_profiles ORDER BY is_default DESC, id LIMIT 1",
    )
    .fetch_one(&state.pool)
    .await
    .unwrap_or(Profile {
        id: 0,
        name: "fallback".into(),
        formats: "epub,azw3,mobi,pdf".into(),
        cutoff: "epub".into(),
        min_size_mb: None,
        max_size_mb: None,
        preferred_words: None,
        must_contain: None,
        must_not_contain: None,
        min_seeders: 1,
        is_default: 1,
    })
}

pub async fn profile_for(state: &crate::AppState, book_profile: Option<i64>) -> Profile {
    if let Some(id) = book_profile {
        if let Ok(Some(p)) =
            sqlx::query_as::<_, Profile>("SELECT * FROM quality_profiles WHERE id=?")
                .bind(id)
                .fetch_optional(&state.pool)
                .await
        {
            return p;
        }
    }
    default_profile(state).await
}

// ---- Settings → Profiles tab ----

use crate::auth::{deny_non_admin, CurrentUser};
use axum::{
    extract::State,
    response::{IntoResponse, Redirect, Response},
    Extension, Form,
};
use maud::html;
use serde::Deserialize;

pub async fn profiles_body(state: &crate::AppState) -> maud::Markup {
    let profiles = sqlx::query_as::<_, Profile>("SELECT * FROM quality_profiles ORDER BY id")
        .fetch_all(&state.pool)
        .await
        .unwrap_or_default();
    html! {
        h2 { "Quality profiles" }
        p .muted { "The format ladder (epub > azw3 > mobi > pdf > txt) replaces the audio codec/bitrate axis. Monitored books below the cutoff get auto-search upgrades every 6 hours." }
        div .results {
            @for p in &profiles {
                div .result {
                    span .title { (p.name) @if p.is_default == 1 { " " span .fmt { "default" } } }
                    span .author {
                        "formats " (p.formats) " · cutoff " (p.cutoff) " · min seeders " (p.min_seeders)
                        @if let Some(w) = &p.preferred_words { " · prefers: " (w) }
                    }
                    @if p.is_default != 1 {
                        form .inline hx-post="/settings/profiles/default" hx-swap="none" {
                            input type="hidden" name="id" value=(p.id);
                            button .link { "make default" }
                        }
                        form .inline hx-post="/settings/profiles/delete" hx-swap="none" hx-confirm={ "Delete profile " (p.name) "?" } {
                            input type="hidden" name="id" value=(p.id);
                            button .link { "remove" }
                        }
                    }
                }
            }
        }
        h2 { "Add profile" }
        form .editform action="/settings/profiles/add" method="post" {
            label { "Name" } input type="text" name="name" required;
            label { "Formats" } input type="text" name="formats" value="epub,azw3,mobi,pdf";
            label { "Cutoff" }
            select name="cutoff" {
                option value="epub" { "epub" }
                option value="azw3" { "azw3" }
                option value="mobi" { "mobi" }
                option value="pdf" { "pdf" }
            }
            label { "Min size MB" } input type="number" step="0.1" name="min_size_mb";
            label { "Max size MB" } input type="number" step="0.1" name="max_size_mb";
            label { "Preferred words" } input type="text" name="preferred_words" placeholder="retail";
            label { "Must contain" } input type="text" name="must_contain";
            label { "Must not contain" } input type="text" name="must_not_contain" placeholder="summary, workbook";
            label { "Min seeders" } input type="number" name="min_seeders" value="1";
            button .btn type="submit" { "Add profile" }
        }
    }
}

#[derive(Deserialize)]
pub struct ProfileForm {
    pub name: String,
    pub formats: Option<String>,
    pub cutoff: Option<String>,
    pub min_size_mb: Option<String>,
    pub max_size_mb: Option<String>,
    pub preferred_words: Option<String>,
    pub must_contain: Option<String>,
    pub must_not_contain: Option<String>,
    pub min_seeders: Option<i64>,
}

pub async fn add(
    State(state): State<crate::AppState>,
    Extension(me): Extension<CurrentUser>,
    Form(f): Form<ProfileForm>,
) -> Response {
    if let Some(r) = deny_non_admin(&me) {
        return r;
    }
    let blank = |s: Option<String>| s.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
    let num = |s: Option<String>| blank(s).and_then(|v| v.parse::<f64>().ok());
    let _ = sqlx::query(
        "INSERT INTO quality_profiles
         (name, formats, cutoff, min_size_mb, max_size_mb, preferred_words, must_contain, must_not_contain, min_seeders)
         VALUES (?,?,?,?,?,?,?,?,?)",
    )
    .bind(f.name.trim())
    .bind(blank(f.formats).unwrap_or_else(|| "epub,azw3,mobi,pdf".into()))
    .bind(blank(f.cutoff).unwrap_or_else(|| "epub".into()))
    .bind(num(f.min_size_mb))
    .bind(num(f.max_size_mb))
    .bind(blank(f.preferred_words))
    .bind(blank(f.must_contain))
    .bind(blank(f.must_not_contain))
    .bind(f.min_seeders.unwrap_or(1))
    .execute(&state.pool)
    .await;
    Redirect::to("/settings?tab=profiles").into_response()
}

#[derive(Deserialize)]
pub struct IdForm {
    pub id: i64,
}

pub async fn delete(
    State(state): State<crate::AppState>,
    Extension(me): Extension<CurrentUser>,
    Form(f): Form<IdForm>,
) -> Response {
    if let Some(r) = deny_non_admin(&me) {
        return r;
    }
    let _ = sqlx::query("DELETE FROM quality_profiles WHERE id=? AND is_default=0")
        .bind(f.id)
        .execute(&state.pool)
        .await;
    (axum::http::StatusCode::OK, "removed").into_response()
}

pub async fn make_default(
    State(state): State<crate::AppState>,
    Extension(me): Extension<CurrentUser>,
    Form(f): Form<IdForm>,
) -> Response {
    if let Some(r) = deny_non_admin(&me) {
        return r;
    }
    let _ = sqlx::query("UPDATE quality_profiles SET is_default = (id=?)")
        .bind(f.id)
        .execute(&state.pool)
        .await;
    (axum::http::StatusCode::OK, "ok").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rel(title: &str, protocol: &str, size: Option<i64>, seeders: Option<i64>) -> Release {
        Release {
            title: title.into(),
            indexer_id: 1,
            indexer: "t".into(),
            protocol: protocol.into(),
            size,
            seeders,
            grabs: None,
            download_url: "u".into(),
            published: None,
        }
    }

    fn profile() -> Profile {
        Profile {
            id: 1,
            name: "std".into(),
            formats: "epub,azw3,mobi,pdf".into(),
            cutoff: "epub".into(),
            min_size_mb: None,
            max_size_mb: Some(200.0),
            preferred_words: Some("retail".into()),
            must_contain: None,
            must_not_contain: Some("summary, workbook".into()),
            min_seeders: 2,
            is_default: 1,
        }
    }

    #[test]
    fn scoring_ladder() {
        let p = profile();
        // epub retail w/ seeders beats bare pdf
        let a = score(&rel("Author - Title (retail) (epub)", "torrent", Some(1 << 20), Some(30)), &p);
        let b = score(&rel("Author - Title (pdf)", "torrent", Some(1 << 20), Some(30)), &p);
        assert!(a.rejected.is_none() && b.rejected.is_none());
        assert!(a.total > b.total, "{} !> {}", a.total, b.total);
    }

    #[test]
    fn hard_rejects() {
        let p = profile();
        assert!(score(&rel("Title Summary epub", "torrent", None, Some(9)), &p).rejected.is_some());
        assert!(score(&rel("Title epub", "torrent", None, Some(1)), &p).rejected.is_some()); // seeders
        assert!(score(&rel("Title epub", "torrent", Some(300 << 20), Some(9)), &p).rejected.is_some()); // size
        // usenet exempt from seeders
        assert!(score(&rel("Title epub", "usenet", None, None), &p).rejected.is_none());
    }

    #[test]
    fn picks_best() {
        let p = profile();
        let rels = vec![
            rel("Title (pdf)", "torrent", None, Some(50)),
            rel("Title (retail) (epub)", "usenet", Some(5 << 20), None),
            rel("Title workbook epub", "torrent", None, Some(99)),
        ];
        let (best, _) = pick_best(&rels, &p).unwrap();
        assert!(best.title.contains("retail"));
    }
}
