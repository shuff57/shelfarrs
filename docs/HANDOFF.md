# shelfarrs — handoff

Last updated: 2026-07-03 (HEAD `284b903`). Repo: https://github.com/shuff57/shelfarrs (public).
Local checkout: `C:\Users\shuff\Documents\GitHub\shelfarrs`.

## What this is

Self-hosted ebook hub — a clean-room Rust rewrite of the Rails Shelfarr fork, built around a
sandboxed WASM plugin ecosystem, with **full Listenarr UI/UX + backend parity remapped for
ebooks**. Single static musl binary, `FROM scratch` Docker image, SQLite, no SPA (axum +
Maud + HTMX). Design docs: `docs/plans/2026-07-02-shelfarrs-design.md` (core architecture)
and `docs/plans/2026-07-02-ebook-backend-parity.md` (acquisition backend).

## Current state — everything below is DONE, tested (19/19), and live-verified

**Core (phases 0–4, earlier):** library + scan, range byte-serving, job queue, extism WASM
source plugins (host-mediated `http_fetch`/`kv`/`log` — plugins get no sockets), JS viewer
plugins (`window.Shelfarrs`), hot plugin install/uninstall from a repo index (sha256 +
zip-slip-guarded extract, zero restart), OPDS, auth/multi-user (argon2 sessions), default
plugin catalog on GitHub Releases (`releases/latest/download/index.json`, seeded by
migration 0005; publish updates with `cargo run --bin package_plugins` →
`gh release create plugins-vX.Y.Z dist/*`).

**Metadata:** scan-time extraction from the files themselves (`src/meta.rs`) — epub OPF
title/author/description/cover (epub3 `cover-image` / epub2 `meta name="cover"` /
name fallback) + series (`calibre:series` and epub3 `belongs-to-collection` +
`group-position`), cbz first-page covers. Online providers (`src/providers.rs`) —
OpenLibrary + Google Books — fill description/cover/ISBN the file lacks, toggleable in
Settings → General. Per-book "Rescan metadata" clears derived fields and re-runs everything.

**Acquisition backend (B1–B6):**
- `src/indexer.rs` — torznab/newznab indexers (one client serves both; NZBGeek preset;
  `t=caps` Test; Prowlarr one-click import via its per-indexer proxy `{prowlarr}/{id}/api`).
  Release search merged into Add New, sorted by seeders.
- `src/downloads.rs` — qBittorrent (`/api/v2`) + SABnzbd (`?mode=`) clients, downloads
  queue, 30s monitor (`queued→downloading→completed→importing→imported|failed`), import
  copies book files into BOOKS_DIR via longest-prefix **remote path mappings**, then scans.
- `src/score.rs` — quality profiles: format ladder (epub 100 > azw3 90 > mobi 80 > pdf 60 >
  txt 30), cutoff, size range, preferred/must/must-not words, min seeders (usenet exempt
  from seeder/size rejects when absent). Base-100 scorer + `pick_best`, unit-tested.
- `src/autosearch.rs` — every 6h (5-min startup delay), monitored books below their
  profile's cutoff get search→score→grab upgrades; Wanted page lists them w/ Search Now.
- Book CRUD: edit panel (patch-style), delete w/ optional file removal (confined to books
  dir), monitored flag, bulk bar on list view, naming pattern
  `{Author}/{Series}/{SeriesNumber}/{Title}` + Organize preview/apply on Library Import.

**UI (measured off the live Listenarr at audiobook.huffpalmer.fyi, not eyeballed):**
60px topbar (collapse-to-icon search w/ HTMX suggestions, bell dropdown, user menu),
200px sidebar (3 sections, 12×16 items @15px, subnavs always in DOM animating
max-height/opacity .22s/.12s — pinned open on active section, hover-revealed otherwise,
HTMX count pills on Activity/Wanted), 36px bordered toolbar buttons @12px text, fixed
190px poster columns + 20px gap, posters = 6px radius + 3px green border-bottom status
line + hover overlay (title/author/monitored) + hover action buttons (read/edit/delete),
captions only behind the info toggle, fixed status legend, detail page = Back + icon
action row + blurred-cover hero + path chip + outline chips + 5-line clamp description
w/ CSS-only Show More + Details/Files/History tabs, tabbed Settings
(Plugins/Indexers/Download Clients/Quality Profiles/Users/General), System stat cards,
Calendar (added-date month grid via sqlite strftime — no chrono dep).

## Running it (dev, this Windows box)

```powershell
cd C:\Users\shuff\Documents\GitHub\shelfarrs
cargo build        # GNU toolchain (no MSVC linker on this machine)
$env:DATA_DIR="data"; $env:BOOKS_DIR="books"; $env:SEED_PLUGINS_DIR="plugins"; $env:BIND="127.0.0.1:8080"
.\target\debug\shelfarrs.exe
```
Dev login `admin`/`admin` (set `SHELFARRS_ADMIN_PASSWORD` in real deploys). Container
builds use the multi-stage Dockerfile (musl in-container — sidesteps the host toolchain).

### Field-proven gotchas
- **`cargo test` does NOT rebuild `target\debug\shelfarrs.exe`** — always `cargo build`
  before relaunching the server or you run a stale binary (bit us twice).
- `r#"…"#` raw strings die on any `"#` inside (SVG `stroke="#hex"`, `refines="#id"`) — use `r##`.
- Indexer URL field tolerates a pasted `…/api` suffix (the client appends `/api` itself).
- Chrome heuristically caches `assets/style.css` — hard-reload after CSS changes.
- The dev DB (`data/shelfarr.db`) currently contains the beelink SAB client + 9
  Prowlarr-imported indexers (with the Prowlarr API key) from live verification — it's
  gitignored; wipe if you want a clean slate.

## Deploying to the beelink (the one remaining TODO — user has deferred it)

New CasaOS app alongside the Rails Shelfarr (same recipe as prior apps: standalone compose
under `/DATA/compose/`, hardcode CasaOS `$vars`). Mount one volume at `/data`
(`DATA_DIR`) and bind `BOOKS_DIR=/media/tank/books`. `SEED_PLUGINS_DIR=/app/plugins` is
baked into the image. After deploy:
1. Settings → Indexers → Import from Prowlarr (`http://192.168.68.246:9696` + its key).
2. Settings → Download Clients → add SABnzbd `192.168.68.246:8090` (API key in the user's
   private notes) and qBittorrent `192.168.68.246:8081` (gluetun-published; LAN auth bypassed).
3. Add **remote path mappings** — the clients save to tank paths the container must see,
   e.g. map SAB's `/downloads/complete` → the container's view of the same tank dir.
4. Set a real `SHELFARRS_ADMIN_PASSWORD`; add `restart: unless-stopped`.

## Known gaps / sensible next steps
- Beelink deploy (above) — everything else is ready for it.
- `plugin_kv` is in-memory (libgen mirror rotation resets on restart) — durable table is
  the designed upgrade.
- Wanted "missing" semantics: today wanted = below-cutoff upgrades + queued acquires; no
  concept of a monitored-but-absent book (needs a wishlist entity if desired).
- CBR covers/viewer (rar) unsupported — convert to CBZ.
- Author metadata pages (photos/bios via OpenLibrary author API) would deepen collections.
- Books folder still holds dev test files (two synthetic "Wonderland Saga" epubs + a test
  PDF) — delete freely.

## Where knowledge lives
- Worker/actor inventory: `main.rs` spawns `jobs::worker` (2s queue poll),
  `downloads::monitor` (30s), `autosearch::worker` (6h).
- Migrations 0001–0012 are append-only, embedded via `sqlx::migrate!()`.
- Plugin SDK types: `crates/sdk`; plugin sources: `plugin-src/{gutenberg,libgen}`.
- The user's private session notes (Claude memory) hold beelink IPs/keys and the full
  verification history; nothing secret is committed to this public repo.
