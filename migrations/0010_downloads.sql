-- B3: download clients (qBittorrent + SABnzbd), remote path mappings, and the
-- downloads queue that the monitor advances: queued -> downloading -> completed
-- -> importing -> imported (or failed).
CREATE TABLE download_clients (
    id              INTEGER PRIMARY KEY,
    name            TEXT NOT NULL,
    kind            TEXT NOT NULL DEFAULT 'qbittorrent',  -- qbittorrent | sabnzbd
    host            TEXT NOT NULL,
    port            INTEGER NOT NULL DEFAULT 8080,
    username        TEXT,
    password        TEXT,          -- qbit password / sab api key
    category        TEXT DEFAULT 'shelfarrs',
    use_ssl         INTEGER NOT NULL DEFAULT 0,
    enabled         INTEGER NOT NULL DEFAULT 1,
    last_test_ok    INTEGER,
    last_test_error TEXT
);

CREATE TABLE path_mappings (
    id          INTEGER PRIMARY KEY,
    client_id   INTEGER NOT NULL,
    remote_path TEXT NOT NULL,   -- as the client reports it (its filesystem)
    local_path  TEXT NOT NULL    -- where shelfarrs can read the same files
);

CREATE TABLE downloads (
    id         INTEGER PRIMARY KEY,
    client_id  INTEGER NOT NULL,
    external_id TEXT,            -- qbit hash / sab nzo_id
    title      TEXT NOT NULL,
    protocol   TEXT NOT NULL,    -- torrent | usenet
    state      TEXT NOT NULL DEFAULT 'queued',
    progress   REAL NOT NULL DEFAULT 0,
    save_path  TEXT,
    error      TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);
