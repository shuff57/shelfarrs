-- Scan-time metadata: description + extracted cover (filename under DATA_DIR/covers),
-- and a one-shot flag so each file is only parsed once.
ALTER TABLE books ADD COLUMN description TEXT;
ALTER TABLE books ADD COLUMN cover TEXT;
ALTER TABLE books ADD COLUMN meta_done INTEGER NOT NULL DEFAULT 0;
