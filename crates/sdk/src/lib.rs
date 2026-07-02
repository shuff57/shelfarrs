//! The plugin ABI, shared verbatim by the host and every plugin. JSON on the wire,
//! so plugins can be written in any language that speaks the same shapes.

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
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    pub bytes: Option<Vec<u8>>,
}

// ---- host imports a plugin may call (the only powers it gets) ----

/// A plugin's request to the host's `http_fetch` — the host does the networking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpRequest {
    #[serde(default = "get")]
    pub method: String,
    pub url: String,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default)]
    pub body: Option<Vec<u8>>,
}

fn get() -> String {
    "GET".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpResponse {
    pub status: u16,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}
