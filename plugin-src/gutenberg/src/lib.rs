//! Project Gutenberg source, as a sandboxed WASM plugin. Networking is delegated
//! to the host's `http_fetch` — the plugin itself gets no socket.

use serde_json::Value;
use shelfarrs_sdk::Candidate;

// The extism glue references host imports that exist only in the WASM runtime, so
// gate it out of host-target test builds. The pure parsing logic below stays testable.
#[cfg(target_arch = "wasm32")]
mod wasm {
    use super::{parse_gutendex, urlencoding};
    use extism_pdk::*;
    use serde_json::Value;
    use shelfarrs_sdk::{Candidate, Download, HttpRequest, HttpResponse, SearchQuery};

    #[host_fn]
    extern "ExtismHost" {
        fn http_fetch(req: Json<HttpRequest>) -> Json<HttpResponse>;
    }

    #[plugin_fn]
    pub fn search(Json(q): Json<SearchQuery>) -> FnResult<Json<Vec<Candidate>>> {
        let url = format!("https://gutendex.com/books/?search={}", urlencoding(&q.text));
        let resp = unsafe {
            http_fetch(Json(HttpRequest {
                method: "GET".into(),
                url,
                headers: vec![],
                body: None,
            }))?
        };
        let HttpResponse { body, .. } = resp.0;
        let json: Value = serde_json::from_slice(&body)?;
        Ok(Json(parse_gutendex(&json)))
    }

    #[plugin_fn]
    pub fn resolve_download(reference: String) -> FnResult<Json<Download>> {
        // reference is already the epub URL; host does the actual byte fetch.
        Ok(Json(Download {
            url: Some(reference),
            headers: vec![],
            bytes: None,
        }))
    }
}

/// Minimal percent-encoding for one query field (no dep).
fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Pull epub candidates out of a Gutendex response. Pure — unit-tested below.
fn parse_gutendex(body: &Value) -> Vec<Candidate> {
    let mut out = vec![];
    let Some(results) = body.get("results").and_then(|r| r.as_array()) else {
        return out;
    };
    for b in results {
        let title = b
            .get("title")
            .and_then(|t| t.as_str())
            .unwrap_or("Untitled")
            .to_string();
        let author = b
            .get("authors")
            .and_then(|a| a.as_array())
            .and_then(|a| a.first())
            .and_then(|a| a.get("name"))
            .and_then(|n| n.as_str())
            .map(str::to_string);
        let epub = b
            .get("formats")
            .and_then(|f| f.as_object())
            .and_then(|f| {
                f.iter()
                    .find(|(k, _)| k.starts_with("application/epub+zip"))
                    .and_then(|(_, v)| v.as_str())
            });
        if let Some(url) = epub {
            out.push(Candidate {
                source: "gutenberg".into(),
                title,
                author,
                format: "epub".into(),
                size: None,
                reference: url.to_string(),
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_epub_candidate() {
        let json = serde_json::json!({
            "results": [
                { "title": "Pride and Prejudice", "authors": [{ "name": "Austen, Jane" }],
                  "formats": { "text/html": "https://x/1.html", "application/epub+zip": "https://x/1.epub" } },
                { "title": "No epub", "authors": [], "formats": { "text/plain": "https://x/2.txt" } }
            ]
        });
        let c = parse_gutendex(&json);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].title, "Pride and Prejudice");
        assert_eq!(c[0].reference, "https://x/1.epub");
    }

    #[test]
    fn urlencodes() {
        assert_eq!(urlencoding("jane austen"), "jane%20austen");
    }
}
