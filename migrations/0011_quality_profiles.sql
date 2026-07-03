-- B4: quality profiles — the ebook analog of the arr codec/bitrate ladder is a
-- format ladder (epub > azw3 > mobi > pdf > txt) plus word/size/seeder rules.
CREATE TABLE quality_profiles (
    id               INTEGER PRIMARY KEY,
    name             TEXT NOT NULL,
    formats          TEXT NOT NULL DEFAULT 'epub,azw3,mobi,pdf', -- allowed
    cutoff           TEXT NOT NULL DEFAULT 'epub',  -- stop upgrading at this format
    min_size_mb      REAL,
    max_size_mb      REAL,
    preferred_words  TEXT,   -- comma list, +5 each
    must_contain     TEXT,
    must_not_contain TEXT,
    min_seeders      INTEGER NOT NULL DEFAULT 1,    -- torrent releases only
    is_default       INTEGER NOT NULL DEFAULT 0
);
INSERT INTO quality_profiles (name, is_default, preferred_words) VALUES ('Standard eBook', 1, 'retail');

ALTER TABLE books ADD COLUMN quality_profile_id INTEGER; -- NULL = default profile
