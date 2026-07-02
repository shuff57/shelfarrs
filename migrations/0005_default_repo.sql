-- Ship with the official plugin repository preconfigured so a fresh install
-- has plugins available out of the box. Insert-only: if the key already exists
-- (user added or removed repos), leave their choice alone.
INSERT INTO settings (key, value)
VALUES ('plugin_repositories', '["https://github.com/shuff57/shelfarrs/releases/latest/download/index.json"]')
ON CONFLICT(key) DO NOTHING;
