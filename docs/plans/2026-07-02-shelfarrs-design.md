# shelfarrs — design (working name)

> Clean-room Rust rewrite of the Shelfarr ebook hub, built around a first-class,
> sandboxed **plugin ecosystem**. Single static binary, Dockerable, self-hosted on
> ZimaOS. `shelfarr-rs` is a placeholder name — rename at repo setup (one find/replace).

Date: 2026-07-02 · Status: design approved, pre-implementation.

## Why this exists

The Rails Shelfarr fork works, but two things drove a rewrite (user weighed the cost
and accepted it, incl. abandoning upstream):

1. **Plugin ecosystem quality.** Runtime-loading backend code in Rails is fragile
   (Zeitwerk/engine/bundler friction). We want a *safe, contributable, language-agnostic*
   plugin system — the kind Rails can't do cleanly.
2. **A stack worth maintaining long-term.** Single-binary Rust that deploys like the
   other self-hosted apps on the beelink.

The Rails app is referenced only as a **feature checklist**, not a code base. No fork
lineage.

## Decisions (locked)

| Decision | Choice |
|---|---|
| Language / stack | **Rust** — axum + Maud + HTMX (no SPA), sqlx + SQLite, extism WASM plugins |
| Deploy | Single static **musl** binary → `FROM scratch`/distroless image; runs on ZimaOS |
| Reader | Stays — shipped as the **flagship viewer plugin**, not core |
| Origin | Clean-room new repo, own name (TBD) |
| Backend plugins | Sandboxed **WASM** (extism); any source language |
| Frontend plugins | Static **JS/CSS** viewer bundles (reader = epub.js/pdf.js/JSZip) |
| Networking | **Host-mediated only** — plugins never get raw sockets |
| Plugin storage | **KV only** (`plugin_kv`) — no plugin tables, no plugin migrations |
| v1 scope | Phased: walking skeleton → prove both plugin types → grow toward parity |

## 1. System architecture

```
┌───────────── single Rust binary (Docker: scratch/distroless) ─────────────┐
│  axum HTTP  ──  Maud server-rendered HTML + HTMX (no SPA)                   │
│    ├─ library / reader / settings / discovery routes                       │
│    ├─ static assets (tower-http): Tailwind CSS, JS viewer bundles           │
│  sqlx + SQLite (one file on config volume)  ── embedded migrations          │
│  job runner: tokio workers + a `jobs` table (claim/poll)   ── no Redis dep  │
│  ┌─ Plugin host (extism/wasmtime) ───────────────────────────────┐         │
│  │  loads plugins/<id>/*.wasm                                      │        │
│  │  exports: search(q) · resolve_download(id)                      │ sandbox│
│  │  imports: http_fetch() · kv_get/set() · log()                   │        │
│  └─────────────────────────────────────────────────────────────────┘       │
│  frontend viewer plugins = static JS/CSS in plugins/<id>/ui/ (reader here)  │
└─────────────────────────────────────────────────────────────────────────────┘
      ▲ plugins/ dir + config DB live on the mounted volume (survive restarts)
```

- **Two plugin kinds, deliberately.** Backend = sandboxed WASM; frontend = static JS.
  Installing either = drop files in `plugins/<id>/` + a manifest.
- **Reloading beats Rails.** A new/updated WASM plugin re-instantiates live (no restart);
  a JS viewer is a fresh asset fetch. Only a *core* upgrade needs a process restart
  (Docker restart policy), same as Listenarr.
- **No heavy deps.** SQLite file, in-process job runner. Add Redis/Postgres only if scale
  ever demands (it won't, at homelab scale).

## 2. Plugin ABI + manifest

**One manifest** — `plugins/<id>/plugin.json`:
```json
{ "id": "libgen", "name": "Library Genesis", "version": "1.0.0",
  "kind": "source",                    // source | discovery | viewer
  "wasm": "libgen.wasm",               // backend kinds
  "ui": null,                          // viewer: {"js":"reader.js","css":"…","formats":["epub","pdf","cbz"]}
  "capabilities": ["http","kv"],       // host powers it may use
  "author": "…", "description": "…" }
```

**Backend contract (WASM via extism, JSON-in/JSON-out).** A source plugin *exports*:
```
search(SearchQuery)     -> [Candidate]   // title, author, format, size, ref
resolve_download(ref)   -> Download      // {url,headers} OR raw bytes
```
Host *imports* (the only powers a plugin gets — sandboxed):
```
http_fetch(req) -> resp   // HOST does the networking, not the plugin
kv_get / kv_set(key,val)  // per-plugin namespaced state
log(level, msg)
```

**Key design choice — plugins never get raw sockets.** All networking goes through the
host's `http_fetch`, centralizing domain rotation / rate-limit / proxy / FlareSolverr in
*one* place and making third-party plugins **safe to install**: a malicious `libgen.wasm`
cannot read your files or hit arbitrary hosts. Strictly better than Rails/Listenarr, where
a backend plugin runs with full process privilege.

**The "SDK" is just a shared types crate** (`SearchQuery`/`Candidate`/`Download`) plus a
JSON schema, so plugins can be written in Go/JS/Python too.

**Frontend viewer contract (JS)** — self-registers against a host global:
```js
Shelfarr.registerViewer({ id:"reader", formats:["epub","pdf","cbz"],
  mount(el, { fileUrl, progress, onProgress }) { /* epub.js / pdf.js */ } })
```
`window.Shelfarr` provides `fileUrl` (core's byte-serving endpoint) + a server-backed
`progress` store. Mirrors Listenarr's window-globals SDK.

## 3. Manager UI + repo-index install

**Settings→Plugins** — Repositories / Available / Installed (installed⊎available, tagged,
*Update* badge on version mismatch).

**Repo-index format** (a raw JSON URL; ships with one default index):
```json
{ "name": "Shelfarr Official",
  "plugins": [ { "id":"libgen", "kind":"source", "version":"1.0.0",
                 "package":"https://…/libgen-1.0.0.zip", "sha256":"…",
                 "capabilities":["http","kv"], "author":"…", "description":"…" } ] }
```

**Install flow:**
```
click Install
  → fetch package zip
  → verify sha256 (integrity)
  → show requested capabilities → user consents        ← permission prompt
  → SafeExtract (zip-slip guard + size caps) → atomic move → plugins/<id>/
  → register:  WASM → host re-instantiates live   ┐  no
               viewer → assets served next open    ┘  restart
```

- **Hot install/uninstall, zero downtime** — the big UX win over Listenarr (which must
  restart the container every install because .NET can't unload an ApplicationPart).
- **Safety = two cheap layers:** `sha256` (integrity) + sandbox with capability consent
  (containment). No code-signing PKI — the sandbox does the heavy lifting.
  <!-- ponytail: add signing only if a real distribution-trust problem appears -->
- **Uninstall:** unregister + delete `plugins/<id>/` (+ its `plugin_kv` rows). Instant.

## 4. Data model & core-vs-plugin boundary

**Core owns the stable spine — only core ever migrates the DB** (embedded sqlx migrations):

| Table / endpoint | Purpose |
|---|---|
| `books` | library: title, author, format, file path (`/media/tank/books`), cover, metadata |
| `jobs` | background queue: scan / search / download / import + status |
| `users`, `settings` | auth + global config |
| `progress` | generic `(user, book, viewer, position_json, percent)` — any viewer persists here |
| `installed_plugins`, `plugin_repositories` | registry state |
| `plugin_kv` | namespaced key→value for every plugin |
| `GET /books/:id/file` | range-request byte serving — the one seam every viewer needs |

**Plugins own only their logic + their `kv` namespace. No plugin touches the schema.**
Biggest simplification over the Rails-engine route: **zero migration coordination** — no
plugin tables, no per-plugin SQLite files, no schema drift.

**Discovery (the one judgment call).** Author-follows / import-lists are relational-ish.
Decision: **store as JSON blobs in `plugin_kv`**, not their own tables. At single-user
homelab scale a few hundred follows is a JSON array — trivial.
<!-- ponytail: discovery state = JSON in plugin_kv. If a plugin ever needs real
     queries/scale, add a "plugin gets its own SQLite file" capability then, not now. -->

**Falls out for free:**
- Reader plugin writes **zero backend code** — serves off `GET /books/:id/file`, persists
  via the `progress` endpoint through the SDK.
- Source plugin persists nothing but a few `kv` settings.
- Uninstall is clean (delete folder + kv rows).

## 5. Phased build plan

Each phase is deployable. The acquisition loop is written **once** against a `Source`
trait in Phase 1; Phase 2 swaps the impl to WASM without rewriting it — no throwaway work.

| Phase | Build | Deployable milestone |
|---|---|---|
| **0. Docker skeleton** | New Rust repo, axum + Maud + Tailwind, sqlx/SQLite, tower-http static; multi-stage Dockerfile → musl static binary → `FROM scratch`. | Container runs on ZimaOS, serves an empty library page. |
| **1. Library + acquisition loop** | `books` + list/detail (HTMX), `GET /books/:id/file` (range), job runner, scan job, and a **`Source` trait** with one in-process impl (Gutenberg, legal). | Import from `/media/tank/books`; search→download→import works. |
| **2. Plugin host (both kinds)** | extism host + shared types crate + `http_fetch`/`kv`/`log`; **move Gutenberg to a WASM plugin** (same trait); **reader JS viewer plugin** (epub) via manifest + `window.Shelfarr`. | Both plugin types load from local `plugins/`. *(the plugin proof)* |
| **3. Manager UI + repo index** | Settings→Plugins, repo-index fetch, install (verify → SafeExtract → hot-reload), capability consent, uninstall. | Install Gutenberg + reader from a repo URL, hot, no restart. |
| **4+. Grow to parity** | libgen source (WASM), discovery (follows/import-lists, kv-backed), pdf/cbz viewers, OPDS, auth/multi-user — each just another plugin or core increment. | Incremental drop-in replacement for the Rails app. |

**Testing** (no framework): `cargo test` on the acquisition loop, `SafeExtract` zip-slip,
ABI serde round-trips; one golden JSON test against the Gutenberg wasm; one integration
test (import sample → serve bytes); playwriter for the visual reader check on beelink.

**Docker / musl.** Build via `cargo-zigbuild` or `rust:alpine` →
`x86_64-unknown-linux-musl` static binary; runtime `FROM scratch` (or distroless) + binary
+ static assets + default `plugins/`. Tiny image.
Note: this Windows box has **no MSVC linker**, so local `cargo run` needs the GNU toolchain
or WSL — but container builds (Linux) sidestep it entirely (same dev pattern as current
Shelfarr, which builds in Docker).

## Open items (not blocking implementation)

- **Name.** `shelfarr-rs` is a placeholder — pick the real one at repo setup.
- **Deploy target.** New CasaOS app on the beelink alongside (not replacing) the Rails
  Shelfarr until parity is reached.
- **Auth / multi-user.** Single admin first (Phase 1); multi-user is a Phase 4 increment.
- **OPDS.** Core or plugin — decide when we reach Phase 4 (leaning core: it's a stable
  read-only surface every reader wants).
