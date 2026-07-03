-- B2: torznab/newznab indexers (core table — multi-instance config + testing
-- are core concerns; WASM source plugins stay for direct sources).
CREATE TABLE indexers (
    id              INTEGER PRIMARY KEY,
    name            TEXT NOT NULL,
    protocol        TEXT NOT NULL DEFAULT 'torrent',  -- torrent | usenet
    url             TEXT NOT NULL,
    api_key         TEXT,
    categories      TEXT DEFAULT '7000,7020',          -- newznab book categories
    priority        INTEGER NOT NULL DEFAULT 25,       -- lower = higher priority
    enabled         INTEGER NOT NULL DEFAULT 1,
    last_test_ok    INTEGER,
    last_test_error TEXT
);
