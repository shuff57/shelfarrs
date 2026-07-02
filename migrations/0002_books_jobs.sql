-- Phase 1: library spine + background job queue.
CREATE TABLE books (
    id       INTEGER PRIMARY KEY,
    title    TEXT NOT NULL,
    author   TEXT,
    format   TEXT NOT NULL,          -- epub | pdf | cbz | mobi | azw3 | txt ...
    path     TEXT NOT NULL UNIQUE,   -- absolute path under BOOKS_DIR
    size     INTEGER,
    added_at TEXT NOT NULL DEFAULT (datetime('now'))
);
-- ponytail: no cover/metadata columns yet; add when the viewer (phase 2) needs them.

CREATE TABLE jobs (
    id         INTEGER PRIMARY KEY,
    kind       TEXT NOT NULL,        -- scan | download
    payload    TEXT NOT NULL,        -- JSON
    status     TEXT NOT NULL DEFAULT 'queued',  -- queued | running | done | failed
    error      TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
CREATE INDEX jobs_status ON jobs(status);
