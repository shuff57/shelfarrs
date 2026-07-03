-- B1: user-editable monitoring flag (drives B4 automatic search).
ALTER TABLE books ADD COLUMN monitored INTEGER NOT NULL DEFAULT 1;
