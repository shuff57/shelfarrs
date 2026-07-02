//! extism WASM plugin host. Loads `plugins/<id>/` (manifest + wasm), grants each
//! plugin exactly the sandboxed host powers it asks for, and exposes source
//! plugins through the `Source` trait.
//!
//! The one design invariant: **plugins never touch a socket or the filesystem.**
//! All networking flows through `http_fetch` here, so a hostile plugin is contained.

use crate::source::Source;
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use extism::{convert::Json, host_fn, Manifest as ExtismManifest, Plugin, PluginBuilder, UserData, ValType, Wasm};
use serde::Deserialize;
use shelfarr_sdk::{Candidate, Download, HttpRequest, HttpResponse, SearchQuery};
use std::collections::HashMap;
use std::io::Read;
use std::path::Path;
use std::sync::{Arc, Mutex};

const PTR: ValType = ValType::I64;

/// Per-plugin host context threaded into the host functions.
pub struct HostCtx {
    plugin_id: String,
    // ponytail: in-memory kv keyed by (plugin, key). Back it with the plugin_kv
    // table when a plugin needs durable state (phase 4 libgen/discovery).
    kv: Arc<Mutex<HashMap<(String, String), String>>>,
}

/// `plugins/<id>/plugin.json`
#[derive(Debug, Deserialize)]
pub struct PluginManifest {
    pub id: String,
    pub name: String,
    pub version: String,
    pub kind: String, // source | discovery | viewer
    #[serde(default)]
    pub wasm: Option<String>,
    #[serde(default)]
    pub ui: Option<ViewerUi>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ViewerUi {
    pub js: String,
    #[serde(default)]
    pub css: Option<String>,
    pub formats: Vec<String>,
}

// ---- host functions: the only powers a plugin gets ----

host_fn!(http_fetch(user_data: HostCtx; req: Json<HttpRequest>) -> Json<HttpResponse> {
    let _ = user_data; // networking needs no per-plugin state
    Ok(Json(do_http(req.0)?))
});

host_fn!(kv_get(user_data: HostCtx; key: String) -> String {
    let ud = user_data.get()?;
    let ud = ud.lock().unwrap();
    let v = ud.kv.lock().unwrap().get(&(ud.plugin_id.clone(), key)).cloned();
    Ok(v.unwrap_or_default())
});

host_fn!(kv_set(user_data: HostCtx; key: String, val: String) {
    let ud = user_data.get()?;
    let ud = ud.lock().unwrap();
    ud.kv.lock().unwrap().insert((ud.plugin_id.clone(), key), val);
    Ok(())
});

host_fn!(log(user_data: HostCtx; msg: String) {
    let ud = user_data.get()?;
    let ud = ud.lock().unwrap();
    tracing::info!(plugin = %ud.plugin_id, "{msg}");
    Ok(())
});

/// Blocking HTTP on behalf of a plugin. ureq (no tokio) since host fns are sync.
fn do_http(req: HttpRequest) -> Result<HttpResponse> {
    let mut r = ureq::request(&req.method, &req.url);
    for (k, v) in &req.headers {
        r = r.set(k, v);
    }
    let resp = match req.body {
        Some(b) => r.send_bytes(&b),
        None => r.call(),
    };
    let resp = match resp {
        Ok(resp) => resp,
        // ureq treats non-2xx as an error but still hands back the response.
        Err(ureq::Error::Status(_, resp)) => resp,
        Err(e) => return Err(anyhow!("http_fetch: {e}")),
    };
    let status = resp.status();
    let headers = resp
        .headers_names()
        .iter()
        .filter_map(|n| resp.header(n).map(|v| (n.clone(), v.to_string())))
        .collect();
    let mut body = vec![];
    resp.into_reader()
        .take(50_000_000) // ponytail: 50MB cap so a plugin can't OOM the host
        .read_to_end(&mut body)?;
    Ok(HttpResponse { status, headers, body })
}

// ---- source plugin behind the Source trait ----

pub struct WasmSource {
    id: String,
    plugin: Arc<Mutex<Plugin>>,
}

impl WasmSource {
    fn load(
        id: &str,
        wasm_path: &Path,
        kv: Arc<Mutex<HashMap<(String, String), String>>>,
    ) -> Result<Self> {
        let manifest = ExtismManifest::new([Wasm::file(wasm_path)]);
        let ud = UserData::new(HostCtx {
            plugin_id: id.to_string(),
            kv,
        });
        let plugin = PluginBuilder::new(manifest)
            .with_wasi(true)
            .with_function("http_fetch", [PTR], [PTR], ud.clone(), http_fetch)
            .with_function("kv_get", [PTR], [PTR], ud.clone(), kv_get)
            .with_function("kv_set", [PTR, PTR], [], ud.clone(), kv_set)
            .with_function("log", [PTR], [], ud.clone(), log)
            .build()
            .with_context(|| format!("instantiating plugin {id}"))?;
        Ok(Self {
            id: id.to_string(),
            plugin: Arc::new(Mutex::new(plugin)),
        })
    }

    async fn call_json(&self, func: &'static str, input: Vec<u8>) -> Result<Vec<u8>> {
        let plugin = self.plugin.clone();
        tokio::task::spawn_blocking(move || {
            let mut p = plugin.lock().unwrap();
            p.call::<Vec<u8>, Vec<u8>>(func, input)
                .map_err(|e| anyhow!("plugin {func}: {e}"))
        })
        .await?
    }
}

#[async_trait]
impl Source for WasmSource {
    fn id(&self) -> &str {
        &self.id
    }
    async fn search(&self, q: &SearchQuery) -> Result<Vec<Candidate>> {
        let out = self.call_json("search", serde_json::to_vec(q)?).await?;
        Ok(serde_json::from_slice(&out)?)
    }
    async fn resolve_download(&self, reference: &str) -> Result<Download> {
        let out = self
            .call_json("resolve_download", reference.as_bytes().to_vec())
            .await?;
        Ok(serde_json::from_slice(&out)?)
    }
}

// ---- discovery of installed plugins ----

/// One installed plugin, as read from disk.
pub struct Installed {
    pub manifest: PluginManifest,
    pub dir: std::path::PathBuf,
}

/// Read every `plugins/<id>/plugin.json`.
pub fn scan_installed(plugins_dir: &Path) -> Vec<Installed> {
    let mut out = vec![];
    let Ok(entries) = std::fs::read_dir(plugins_dir) else {
        return out;
    };
    for e in entries.flatten() {
        let dir = e.path();
        let mf = dir.join("plugin.json");
        if !mf.is_file() {
            continue;
        }
        match std::fs::read_to_string(&mf).and_then(|s| {
            serde_json::from_str::<PluginManifest>(&s)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
        }) {
            Ok(manifest) => out.push(Installed { manifest, dir }),
            Err(e) => tracing::warn!("bad manifest {}: {e}", mf.display()),
        }
    }
    out
}

/// The first installed viewer plugin whose manifest declares `format`.
pub fn viewer_for(plugins_dir: &Path, format: &str) -> Option<PluginManifest> {
    scan_installed(plugins_dir)
        .into_iter()
        .map(|i| i.manifest)
        .find(|m| {
            m.kind == "viewer"
                && m.ui.as_ref().is_some_and(|ui| ui.formats.iter().any(|f| f == format))
        })
}

pub type SourceMap = HashMap<String, Arc<dyn Source>>;
pub type KvStore = Arc<Mutex<HashMap<(String, String), String>>>;

/// Instantiate all `kind:"source"` plugins found under `plugins_dir`.
pub fn load_sources(plugins_dir: &Path, kv: KvStore) -> SourceMap {
    let mut sources: SourceMap = HashMap::new();
    for inst in scan_installed(plugins_dir) {
        if inst.manifest.kind != "source" {
            continue;
        }
        let Some(wasm) = &inst.manifest.wasm else {
            tracing::warn!("source plugin {} has no wasm", inst.manifest.id);
            continue;
        };
        let wasm_path = inst.dir.join(wasm);
        match WasmSource::load(&inst.manifest.id, &wasm_path, kv.clone()) {
            Ok(src) => {
                tracing::info!("loaded source plugin: {} v{}", inst.manifest.id, inst.manifest.version);
                sources.insert(inst.manifest.id.clone(), Arc::new(src));
            }
            Err(e) => tracing::error!("failed to load {}: {e:#}", inst.manifest.id),
        }
    }
    sources
}
