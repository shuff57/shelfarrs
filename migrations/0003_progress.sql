-- Phase 2: generic reading progress. Any viewer persists here through /progress.
CREATE TABLE progress (
    user          TEXT    NOT NULL DEFAULT 'default',  -- single admin until phase 4
    book          INTEGER NOT NULL,
    viewer        TEXT    NOT NULL,
    position_json TEXT    NOT NULL,   -- viewer-defined (epub: {cfi, percent})
    percent       REAL,
    updated_at    TEXT    NOT NULL DEFAULT (datetime('now')),
    PRIMARY KEY (user, book)
);
