//! Builds dist/: one zip per bundled plugin + the repo index.json the manager
//! consumes. Publish dist/* as release assets; `releases/latest/download/` is
//! the stable base URL (also the default arg).
//! Usage: cargo run --bin package_plugins [-- <package-base-url>]

use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};

const DEFAULT_BASE: &str = "https://github.com/shuff57/shelfarrs/releases/latest/download";

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e: &std::fs::DirEntry| e.path());
    for e in entries {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out)?;
        } else {
            out.push(p);
        }
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn main() -> anyhow::Result<()> {
    let base = std::env::args().nth(1).unwrap_or_else(|| DEFAULT_BASE.into());
    let dist = Path::new("dist");
    std::fs::create_dir_all(dist)?;

    let mut plugins = vec![];
    let mut dirs: Vec<_> = std::fs::read_dir("plugins")?.collect::<Result<Vec<_>, _>>()?;
    dirs.sort_by_key(|e| e.path());
    for d in dirs {
        let dir = d.path();
        let mf_path = dir.join("plugin.json");
        if !dir.is_dir() || !mf_path.is_file() {
            continue; // .tmp-* leftovers, stray files
        }
        let mf: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&mf_path)?)?;
        let id = mf["id"].as_str().expect("manifest id").to_string();
        let version = mf["version"].as_str().expect("manifest version").to_string();

        let mut files = vec![];
        walk(&dir, &mut files)?;
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut w = zip::ZipWriter::new(&mut buf);
            let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default();
            for f in &files {
                // forward slashes: zip entry names are /-separated regardless of host OS
                let rel = f.strip_prefix(&dir)?.to_string_lossy().replace('\\', "/");
                w.start_file(rel, opts)?;
                w.write_all(&std::fs::read(f)?)?;
            }
            w.finish()?;
        }
        let bytes = buf.into_inner();
        let zip_name = format!("{id}-{version}.zip");
        let sha = hex(&Sha256::digest(&bytes));
        std::fs::write(dist.join(&zip_name), &bytes)?;
        println!("packaged {zip_name}  sha256={sha}");

        plugins.push(serde_json::json!({
            "id": id, "kind": mf["kind"], "version": version,
            "package": format!("{base}/{zip_name}"),
            "sha256": sha,
            "capabilities": mf.get("capabilities").cloned().unwrap_or_else(|| serde_json::json!([])),
            "author": mf.get("author").cloned().unwrap_or(serde_json::Value::Null),
            "description": mf.get("description").cloned().unwrap_or(serde_json::Value::Null),
        }));
    }

    let index = serde_json::json!({ "name": "Shelfarrs Official", "plugins": plugins });
    std::fs::write(dist.join("index.json"), serde_json::to_vec_pretty(&index)?)?;

    // self-check: every zip on disk re-hashes to the sha256 recorded in index.json
    let idx: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dist.join("index.json"))?)?;
    let entries = idx["plugins"].as_array().unwrap();
    for e in entries {
        let name = format!("{}-{}.zip", e["id"].as_str().unwrap(), e["version"].as_str().unwrap());
        let got = hex(&Sha256::digest(&std::fs::read(dist.join(&name))?));
        assert_eq!(got, e["sha256"].as_str().unwrap(), "sha mismatch for {name}");
    }
    println!("index.json OK ({} plugins, self-check passed)", entries.len());
    Ok(())
}
