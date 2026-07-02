//! The acquisition seam. Written once against a `Source` trait so phase 2 can swap
//! the in-process Gutenberg impl for a sandboxed WASM plugin without touching the
//! job runner that calls it.

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchQuery {
    pub text: String,
    pub format: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    pub source: String,
    pub title: String,
    pub author: Option<String>,
    pub format: String,
    pub size: Option<u64>,
    /// Opaque handle the source understands (a URL, an id, …).
    pub reference: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Download {
    /// Host fetches this if `bytes` is None.
    pub url: Option<String>,
    pub headers: Vec<(String, String)>,
    pub bytes: Option<Vec<u8>>,
}

#[async_trait]
pub trait Source: Send + Sync {
    fn id(&self) -> &str;
    async fn search(&self, q: &SearchQuery) -> Result<Vec<Candidate>>;
    async fn resolve_download(&self, reference: &str) -> Result<Download>;
}

/// Project Gutenberg via the Gutendex JSON API — a legal, in-process source.
pub struct Gutenberg {
    http: reqwest::Client,
}

impl Gutenberg {
    pub fn new(http: reqwest::Client) -> Self {
        Self { http }
    }
}

#[async_trait]
impl Source for Gutenberg {
    fn id(&self) -> &str {
        "gutenberg"
    }

    async fn search(&self, q: &SearchQuery) -> Result<Vec<Candidate>> {
        let url = format!(
            "https://gutendex.com/books/?search={}",
            urlencoding(&q.text)
        );
        let body: serde_json::Value = self.http.get(url).send().await?.json().await?;
        Ok(parse_gutendex(&body))
    }

    async fn resolve_download(&self, reference: &str) -> Result<Download> {
        // reference is already the epub URL.
        Ok(Download {
            url: Some(reference.to_string()),
            headers: vec![],
            bytes: None,
        })
    }
}

/// Minimal percent-encoding for the query string (no dep for one field).
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
fn parse_gutendex(body: &serde_json::Value) -> Vec<Candidate> {
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
        // Prefer a plain epub download URL.
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
    fn parses_epub_candidate_from_gutendex() {
        let json = serde_json::json!({
            "results": [
                {
                    "title": "Pride and Prejudice",
                    "authors": [{ "name": "Austen, Jane" }],
                    "formats": {
                        "text/html": "https://x/1342.html",
                        "application/epub+zip": "https://x/1342.epub"
                    }
                },
                { "title": "No epub here", "authors": [], "formats": { "text/plain": "https://x/2.txt" } }
            ]
        });
        let c = parse_gutendex(&json);
        assert_eq!(c.len(), 1, "only the entry with an epub is a candidate");
        assert_eq!(c[0].title, "Pride and Prejudice");
        assert_eq!(c[0].author.as_deref(), Some("Austen, Jane"));
        assert_eq!(c[0].format, "epub");
        assert_eq!(c[0].reference, "https://x/1342.epub");
    }

    #[test]
    fn urlencodes_spaces_and_symbols() {
        assert_eq!(urlencoding("jane austen"), "jane%20austen");
        assert_eq!(urlencoding("a&b"), "a%26b");
    }
}
