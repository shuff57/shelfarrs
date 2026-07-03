# shelfarrs — full backend parity plan (ebooks, not audiobooks)

Date: 2026-07-02 · Status: designed from a deep review of the Listenarr fork's backend
(`Developer/Listenarr`: domain/application/infrastructure/api). UI parity shipped already
(commit 32be427); this plan covers the acquisition/metadata/CRUD backend. Facts below are
Listenarr's architecture; every "shelfarrs:" line is our original Rust mapping.

## The acquisition loop we're building

```mermaid
flowchart LR
    W[Wanted / monitored books] -->|every 6h + manual| S[Search all enabled indexers]
    S -->|torznab/newznab GET api?t=search| R[Releases: title,size,seeders,protocol,url]
    R --> SC[Score vs quality profile]
    SC -->|top non-rejected| G[Grab]
    G -->|torrent| QB[qBittorrent API]
    G -->|nzb| SAB[SABnzbd API]
    QB & SAB -->|poll ~30s| M[Download monitor]
    M -->|completed| I[Import: path-map, move/copy, name, register]
    I --> L[Library + metadata enrich]
```

## 1. Indexers (torrent + NZBGeek/newznab) — CORE, not plugins

Listenarr facts: one `Indexer` table (name, type Torrent|Usenet, implementation
Newznab|Torznab, url, api key, categories, priority, min-age, retention, max-size,
enable flags, last-test state). ONE provider implementation serves both torznab and
newznab — `GET {url}/api?t=search&q=…&apikey=…&cat=…&limit=100&extended=1`, XML RSS
response with torznab attrs (size/seeders/peers/grabs). Endpoints: CRUD + `/test` +
`/toggle` + Prowlarr import.

shelfarrs: **indexers are a core table + one Rust torznab client**, NOT WASM plugins —
multi-instance config, testing, and priorities are core concerns (same reason Listenarr
keeps them core). WASM source plugins stay for direct sources (gutenberg, libgen).

- Migration: `indexers(id, name, protocol torrent|usenet, url, api_key, categories,
  priority, min_age, max_size_mb, enabled, last_test_ok, last_test_error)`.
- `src/indexer.rs`: build url → fetch (existing reqwest client) → parse XML RSS.
  ponytail: hand-rolled tag scan like meta.rs — torznab RSS is machine-generated.
- Release struct: title, size, seeders/leechers, grabs, protocol, download_url
  (magnet | .torrent | .nzb), indexer_id, published, category.
- Settings → new **Indexers tab**: list + add/edit form (name, url, api key,
  categories, priority, enabled) + Test button (t=caps ping) + NZBGeek preset
  (url `https://api.nzbgeek.info`, just paste the key).
- Prowlarr import: later, single endpoint pulling its indexer list (user runs Prowlarr).

## 2. Download clients — qBittorrent first, SABnzbd second

Listenarr facts: client table (name, type, host, port, user/pass, category,
use_ssl, remove-completed policy, settings json). qBittorrent via `/api/v2/`
(auth/login → torrents/add multipart → setCategory → torrents/info poll →
torrents/delete). SABnzbd via `?mode=addurl|queue|history`. Remote path mapping
table (client_id, remote_path, local_path) for container mount mismatches.
Poll ~30s/client w/ backoff; on complete → import job (move/copy per setting,
naming service, register file, cleanup per policy).

shelfarrs:
- Migration: `download_clients(id, name, kind qbittorrent|sabnzbd, host, port,
  username, password, category, use_ssl, enabled, remove_completed)` +
  `path_mappings(client_id, remote_path, local_path)`.
- `src/client_qbit.rs` (login/add/info/delete — the beelink runs qBit behind
  gluetun:8080, path mapping is REQUIRED there), `src/client_sab.rs` later.
- `downloads` table (release title, client id, hash/nzo_id, book_id?, progress,
  state queued|downloading|completed|importing|imported|failed) — feeds the
  existing Activity page's progress bars with real percentages.
- Job worker grows a 30s poll tick (tokio interval alongside the 2s queue poll).
- Import: path-map → move/copy into books dir → scan+enrich (already built).
- Settings → new **Download Clients tab**: CRUD + Test.

## 3. Quality profiles + scoring — the ebook analog

Listenarr facts: profile = allowed qualities + cutoff + size min/max + preferred
words / must-contain / must-not-contain + preferred languages + min seeders +
min score. Scorer: base 100; hard rejects (forbidden word, size range, seeders,
age, quality); ± bonuses (preferred words +5, seeders +≤10, format mismatch −12,
language mismatch −15, age up to −60); usenet exempt from seeder/size penalties.
Auto-search: every 6h, monitored books below cutoff w/o active download, grab
single top-scored release.

shelfarrs ebook axis — replace codec/bitrate ladder with:
- Format ladder: epub 100, azw3 90, mobi 80, pdf 60, txt 30 (cutoff default epub).
- "Retail" preferred-word bonus preset; same must/must-not/size/seeders/language knobs.
- Migration: `quality_profiles(id, name, formats_json, cutoff, min_size_mb,
  max_size_mb, preferred_words, must_contain, must_not_contain, min_seeders,
  is_default)` + `books.monitored INTEGER` + `books.quality_profile_id`.
- `src/score.rs` pure fn + unit tests (golden release lists).
- Auto-search worker: 6h tokio interval; Wanted page shows monitored-missing
  books w/ per-book Search Now (already has the button shape).

## 4. Metadata providers — OpenLibrary + Google Books

Listenarr facts: strategy interface per provider (Audnexus/Audible/OpenLibrary),
coordinator merges; per-book rescan endpoint + identifier (ASIN/ISBN) replace;
5-min background enrichment; provider endpoints stored as ApiConfiguration rows.

shelfarrs: ebook analogs = **OpenLibrary** (free, ISBN-first) + **Google Books**
(better descriptions; optional key). File extraction (built) stays first; online
providers fill gaps + power re-match.
- `src/providers.rs`: `trait MetadataProvider { search(q); by_isbn(isbn) }` — a
  plain Rust trait, two impls; host-side (needs no sandbox: our own code).
- `books.isbn` column; detail page "Rescan metadata" + "Edit identifiers".
- Enrichment: extend existing `enrich_pending` — after file extraction, if
  description/cover still missing and a provider is enabled, look up by
  title+author (or ISBN) and fill. Settings → General grows a Metadata section
  (enable per provider, Google key).

## 5. Book CRUD — edit / delete / descriptions

Listenarr facts: `PUT library/{id}` patches title/authors/description/series/
tags/monitored/profile/etc (null = keep); `DELETE library/{id}?deleteFiles=&deleteFolder=`;
bulk delete/update; rename preview/execute.

shelfarrs (FIRST increment of this plan — smallest, pure-core):
- Detail page **Edit** drawer: title, author, series, series_index, description,
  monitored, profile → `POST /books/{id}/edit` (COALESCE-style patch).
- **Delete** button w/ confirm + "also delete file from disk" checkbox →
  `POST /books/{id}/delete` (removes row + cover + optionally the file).
- Bulk select/edit on library grid: after singles work.

## 6. Settings, root folders, naming

- Settings storage: keep our `settings` KV table (finer-grained than Listenarr's
  one-wide-row; no migration churn per new key).
- Root folders: table later — single BOOKS_DIR is fine until multi-library.
- Naming: `{Author}/{Series}/{Title}.{ext}` pattern service + Organize
  (preview → move) — after download clients land (imports need it first).

## Build order (each phase deployable, mirrors how the UI was grown)

| Phase | Scope | Parity unlocked |
|---|---|---|
| **B1** | Book CRUD: edit drawer, delete w/ file option, monitored flag | edits/deletions/descriptions |
| **B2** | Indexers: table + torznab/newznab client + Settings tab + Test + NZBGeek preset; interactive release search in Add New (seeders/size/protocol columns) | torrent + NZBGeek sourcing |
| **B3** | qBittorrent client + downloads table + 30s monitor + import pipeline + path mappings; real Activity progress | grabbing + queue |
| **B4** | Quality profiles + scorer + 6h auto-search of monitored books | hands-off automation |
| **B5** | OpenLibrary/Google Books providers + ISBN + rescan/re-match | metadata providers |
| **B6** | SABnzbd, naming/organize, bulk edit, Prowlarr import | long tail |

Non-goals for now: SignalR-style live push (HTMX polling covers it), Discord bot,
ffmpeg (audio-only concern), per-user quality settings.
