//! Library Genesis source, as a sandboxed WASM plugin. Networking is host-mediated
//! (http_fetch); the active mirror host is remembered in kv (rotates on empty result).
//!
//! ponytail: hand-rolled HTML scan (fixed patterns, no regex dep -> small wasm). If
//! libgen changes its table markup, update the finders here — that's the known ceiling.

use shelfarrs_sdk::Candidate;

const MIRRORS: &[&str] = &["libgen.li", "libgen.gs", "libgen.la"];
const EXTS: &[&str] = &["epub", "pdf", "mobi", "azw3", "cbr", "cbz", "djvu", "txt", "fb2"];

fn unescape(s: &str) -> String {
    s.replace("&amp;", "&").replace("&#39;", "'").replace("&quot;", "\"")
}

fn find_md5(row: &str) -> Option<String> {
    let i = row.find("md5=")? + 4;
    let hex: String = row[i..].chars().take(32).collect();
    if hex.len() == 32 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        Some(hex.to_lowercase())
    } else {
        None
    }
}

/// Anchor text of the first *non-empty* edition.php link = the title. (The cover
/// image is also an edition.php link, so skip the empty one.)
fn find_title(row: &str) -> Option<String> {
    let mut rest = row;
    while let Some(a) = rest.find("href=\"edition.php") {
        if let Some(gt) = rest[a..].find("\">").map(|x| x + a + 2) {
            if let Some(end) = rest[gt..].find('<').map(|x| x + gt) {
                let t = unescape(rest[gt..end].trim());
                if !t.is_empty() {
                    return Some(t);
                }
                rest = &rest[end..];
                continue;
            }
        }
        rest = &rest[a + 17..];
    }
    None
}

/// A bare `<td>epub</td>` cell.
fn find_ext(row: &str) -> Option<String> {
    for ext in EXTS {
        if row.contains(&format!("<td>{ext}</td>")) {
            return Some((*ext).to_string());
        }
    }
    None
}

/// First pure-text `<td>` that looks like an author list (contains ';', no tags).
fn find_author(row: &str) -> Option<String> {
    let mut rest = row;
    while let Some(o) = rest.find("<td>") {
        let start = o + 4;
        let end = rest[start..].find("</td>")? + start;
        let cell = &rest[start..end];
        if cell.contains(';') && !cell.contains('<') {
            let first: String = cell.split(';').next().unwrap_or(cell).chars().take(80).collect();
            let a = unescape(first.trim());
            if !a.is_empty() {
                return Some(a);
            }
        }
        rest = &rest[end + 5..];
    }
    None
}

/// Parse a libgen.li results page into candidates. Pure — unit-tested below.
fn parse_libgen(html: &str) -> Vec<Candidate> {
    let mut out = vec![];
    for row in html.split("<tr").skip(1) {
        if !row.contains("ads.php?md5=") && !row.contains("get.php?md5=") {
            continue;
        }
        let Some(md5) = find_md5(row) else { continue };
        let title = find_title(row).unwrap_or_else(|| "Untitled".into());
        let Some(format) = find_ext(row) else { continue };
        out.push(Candidate {
            source: "libgen".into(),
            title,
            author: find_author(row),
            format,
            size: None,
            reference: md5,
        });
    }
    out
}

/// The get.php download URL from an ads.php page.
fn find_get_link(html: &str) -> Option<String> {
    let i = html.find("get.php?md5=")?;
    let end = html[i..].find('"')? + i;
    Some(unescape(&html[i..end]))
}

fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(target_arch = "wasm32")]
mod wasm {
    use super::*;
    use extism_pdk::*;
    use shelfarrs_sdk::{Download, HttpRequest, HttpResponse, SearchQuery};

    #[host_fn]
    extern "ExtismHost" {
        fn http_fetch(req: Json<HttpRequest>) -> Json<HttpResponse>;
        fn kv_get(key: String) -> String;
        fn kv_set(key: String, val: String);
    }

    fn host() -> String {
        let h = unsafe { kv_get("host".into()) }.unwrap_or_default();
        if h.is_empty() { MIRRORS[0].to_string() } else { h }
    }

    fn get_html(url: &str) -> Result<String, Error> {
        let resp = unsafe {
            http_fetch(Json(HttpRequest { method: "GET".into(), url: url.into(), headers: vec![], body: None }))?
        };
        Ok(String::from_utf8_lossy(&resp.0.body).into_owned())
    }

    #[plugin_fn]
    pub fn search(Json(q): Json<SearchQuery>) -> FnResult<Json<Vec<Candidate>>> {
        // Try the remembered host first, then rotate through mirrors on empty result.
        let start = host();
        let ordered: Vec<&str> = std::iter::once(start.as_str())
            .chain(MIRRORS.iter().copied().filter(|m| *m != start))
            .collect();
        for h in ordered {
            let url = format!("https://{h}/index.php?req={}&res=50", urlencoding(&q.text));
            let Ok(html) = get_html(&url) else { continue };
            let mut cands = parse_libgen(&html);
            if let Some(fmt) = &q.format {
                cands.retain(|c| &c.format == fmt);
            }
            if !cands.is_empty() {
                let _ = unsafe { kv_set("host".into(), h.into()) }; // remember the working mirror
                return Ok(Json(cands));
            }
        }
        Ok(Json(vec![]))
    }

    #[plugin_fn]
    pub fn resolve_download(md5: String) -> FnResult<Json<Download>> {
        let h = host();
        let ads = get_html(&format!("https://{h}/ads.php?md5={md5}"))?;
        let link = find_get_link(&ads)
            .ok_or_else(|| WithReturnCode::new(Error::msg("no download link on ads page"), 1))?;
        let url = if link.starts_with("http") {
            link
        } else {
            format!("https://{h}/{}", link.trim_start_matches('/'))
        };
        Ok(Json(Download { url: Some(url), headers: vec![], bytes: None }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROW: &str = r#"<tr> <td><a href="edition.php?id=1"><img></a></td>
      <td><a href="edition.php?id=1">The theology of Dracula <i></i></a></td>
      <td>Dracula;Stoker, Bram;Rarignac</td> <td>McFarland &amp; Co</td>
      <td>2012</td> <td>English</td> <td>241 pages</td>
      <td><a href="/file.php?id=9">5 MB</a></td> <td>pdf</td>
      <td><a href="/ads.php?md5=78e0dcf53cc3483ee60c8aa2f5779e1a">libgen</a></td> </tr>"#;

    #[test]
    fn parses_a_row() {
        let c = parse_libgen(ROW);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].title, "The theology of Dracula");
        assert_eq!(c[0].format, "pdf");
        assert_eq!(c[0].reference, "78e0dcf53cc3483ee60c8aa2f5779e1a");
        assert_eq!(c[0].author.as_deref(), Some("Dracula"));
    }

    #[test]
    fn skips_rows_without_a_book() {
        assert!(parse_libgen("<tr><td>header</td></tr>").is_empty());
    }

    #[test]
    fn extracts_get_link() {
        let ads = r#"<a href="get.php?md5=abc&amp;key=KEY9">GET</a>"#;
        assert_eq!(find_get_link(ads).as_deref(), Some("get.php?md5=abc&key=KEY9"));
    }
}
