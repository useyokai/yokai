CREATE TABLE instance_meta (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    instance_id BLOB NOT NULL CHECK (length(instance_id) = 16),
    format INTEGER NOT NULL CHECK (format = 1),
    created_at TEXT NOT NULL,
    last_health_check TEXT
) STRICT;
PRAGMA user_version = 1;
