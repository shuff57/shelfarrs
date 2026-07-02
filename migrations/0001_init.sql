-- Core spine, phase 0: prove the migration pipeline.
-- Core owns settings (global config); plugins never touch the schema.
CREATE TABLE settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
