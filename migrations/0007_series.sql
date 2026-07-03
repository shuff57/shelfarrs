-- Series metadata from epub OPF (calibre:series / belongs-to-collection).
ALTER TABLE books ADD COLUMN series TEXT;
ALTER TABLE books ADD COLUMN series_index REAL;
-- Re-enrich existing books on the next scan so series backfills.
UPDATE books SET meta_done = 0;
