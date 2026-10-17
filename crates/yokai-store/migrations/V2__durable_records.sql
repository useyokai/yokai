CREATE TABLE runtime_epoch (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    epoch INTEGER NOT NULL CHECK (epoch >= 1),
    clean_shutdown INTEGER NOT NULL CHECK (clean_shutdown IN (0, 1))
) STRICT;
INSERT INTO runtime_epoch VALUES (1, 1, 0);

CREATE TABLE runs (
    run_id INTEGER PRIMARY KEY,
    admission TEXT NOT NULL CHECK (admission IN ('Queued', 'WaitingApproval', 'Admitted', 'Held')),
    status TEXT NOT NULL CHECK (status IN ('Open', 'Completed', 'Failed', 'Cancelled')),
    hold_reason TEXT CHECK (hold_reason IN ('StartupInterrupted', 'RecoveryRequired', 'ApprovalInvalidated')),
    recovery_required INTEGER NOT NULL DEFAULT 0 CHECK (recovery_required IN (0, 1)),
    admitted_epoch INTEGER,
    agent_revision TEXT NOT NULL DEFAULT 'rev-1' CHECK (length(agent_revision) BETWEEN 1 AND 64),
    created_epoch INTEGER NOT NULL,
    CHECK ((admission = 'Held') = (hold_reason IS NOT NULL)),
    CHECK (admission <> 'Admitted' OR admitted_epoch IS NOT NULL)
) STRICT;

CREATE TABLE commands (
    command_id TEXT NOT NULL PRIMARY KEY CHECK (length(CAST(command_id AS BLOB)) BETWEEN 1 AND 128),
    kind TEXT NOT NULL CHECK (length(CAST(kind AS BLOB)) BETWEEN 1 AND 64),
    op_version INTEGER NOT NULL CHECK (op_version BETWEEN 1 AND 65535),
    payload BLOB NOT NULL CHECK (length(payload) <= 65536),
    request_digest BLOB NOT NULL CHECK (length(request_digest) = 32),
    run_id INTEGER NOT NULL UNIQUE REFERENCES runs(run_id),
    accepted_epoch INTEGER NOT NULL
) STRICT;
CREATE TRIGGER commands_immutable BEFORE UPDATE ON commands
BEGIN SELECT RAISE(ABORT, 'commands are immutable'); END;
CREATE TRIGGER commands_append_only BEFORE DELETE ON commands
BEGIN SELECT RAISE(ABORT, 'commands are append-only'); END;
-- REPLACE may skip DELETE triggers when recursive_triggers is OFF. Refuse before
-- either the command identity or unique run binding can cause implicit deletion.
CREATE TRIGGER commands_no_replace BEFORE INSERT ON commands
WHEN EXISTS (SELECT 1 FROM commands WHERE command_id = NEW.command_id OR run_id = NEW.run_id)
BEGIN SELECT RAISE(ABORT, 'commands are append-only'); END;

CREATE TABLE jobs (
    job_id INTEGER PRIMARY KEY,
    run_id INTEGER NOT NULL REFERENCES runs(run_id),
    job_no INTEGER NOT NULL CHECK (job_no >= 1),
    state TEXT NOT NULL CHECK (state IN ('Pending', 'Claimed', 'Done', 'Cancelled')),
    claimed_by TEXT,
    claim_epoch INTEGER,
    created_epoch INTEGER NOT NULL,
    UNIQUE (run_id, job_no),
    UNIQUE (job_id, run_id),
    CHECK (state <> 'Pending' OR (claimed_by IS NULL AND claim_epoch IS NULL)),
    CHECK (state NOT IN ('Claimed', 'Done') OR (claimed_by IS NOT NULL AND claim_epoch IS NOT NULL))
) STRICT;

CREATE TABLE events (
    seq INTEGER PRIMARY KEY,
    run_id INTEGER REFERENCES runs(run_id),
    kind TEXT NOT NULL,
    detail TEXT NOT NULL DEFAULT '',
    epoch INTEGER NOT NULL
) STRICT;
CREATE UNIQUE INDEX events_one_accept_per_run ON events(run_id) WHERE kind = 'run.accepted';
PRAGMA user_version = 2;
