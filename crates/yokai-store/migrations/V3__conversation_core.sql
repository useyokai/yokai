CREATE TABLE conversations (
    conversation_id BLOB PRIMARY KEY CHECK (length(conversation_id) = 16),
    create_run_id INTEGER NOT NULL UNIQUE REFERENCES runs(run_id)
) STRICT;

CREATE TABLE conversation_runs (
    run_id INTEGER PRIMARY KEY REFERENCES runs(run_id),
    conversation_id BLOB NOT NULL REFERENCES conversations(conversation_id),
    active INTEGER NOT NULL CHECK (active IN (0, 1))
) STRICT;
CREATE UNIQUE INDEX conversation_one_active_run ON conversation_runs(conversation_id) WHERE active = 1;
CREATE INDEX conversation_run_order ON conversation_runs(conversation_id, run_id);

CREATE TABLE conversation_messages (
    message_id BLOB PRIMARY KEY CHECK (length(message_id) = 16),
    run_id INTEGER NOT NULL REFERENCES conversation_runs(run_id),
    role TEXT NOT NULL CHECK (role IN ('user', 'assistant')),
    body TEXT NOT NULL CHECK (
        length(CAST(body AS BLOB)) BETWEEN 1 AND 65536
        AND (role = 'assistant' OR length(CAST(body AS BLOB)) <= 16384)
    ),
    UNIQUE (run_id, role)
) STRICT;

CREATE INDEX events_run_sequence ON events(run_id, seq);
PRAGMA user_version = 3;
