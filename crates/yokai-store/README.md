# Instance schema migrations

The current schema version is **2**. V1 contains the STRICT singleton
`instance_meta` table and the owned runner's `yokai_schema_history` table.
V2 adds STRICT `runtime_epoch`, `runs`, `commands`, `jobs` and `events` tables.
Jobs are records only. Admission, claim, execution and dispatch have no crate API.

SQL lives in `migrations/V{n}__{name}.sql` and is embedded at compile time.
Each migration ends with its matching `PRAGMA user_version = n;`. The
conservative lint rejects transaction control, connection-changing statements
and quoted identifiers; only the exact command refusal trigger grammar is allowed.
Migration SQL, its history row, version and the V1
bound identity insert commit in one owned transaction. Checksums detect drift;
they are not protection against a writer who can replace the database.

Opening an Instance holds its exclusive lock before compatibility checks or
migration. WAL compatibility queries share a read-only SQLite transaction.
Without WAL, raw-header refusal precedes writable Stage B; other compatibility
failures report Stage B's observed effects. Current schemas are validated
without updating health timestamps. Newer versions, changed history, unexpected
schema and mismatched identity are refused without automatic repair.

New databases need no recovery snapshot. Every migration of an existing
database first creates `backups/pre-v{old}-to-v{latest}-{utc}-{nonce}.db` using
exclusive creation and `VACUUM INTO`, syncs the file and checks it read-only for
integrity, foreign keys and its original version. Before snapshot creation,
available volume space must reach twice the logical database size plus 16MiB.
That sample is not a reservation and cannot guarantee against ENOSPC.

Snapshots, including partial failed attempts, are never automatically deleted.
`Instance::backup_usage()` reports their total bytes and a provisional 1GiB
warning threshold. Removal is a manual maintenance decision.
An INFO maintenance record reports total bytes after every verified snapshot;
a WARN record is emitted above the threshold, before migration starts. A report
write failure aborts migration and retains its snapshot. These records include
the Instance ID and byte totals, without payloads or filesystem paths.
Windows directory flush remains unsupported, so this does not establish
power-loss durability.
Free-space measurement is implemented for Windows and x86_64 glibc Linux;
other targets fail closed before upgrade. Native Linux validation is separate
from Windows validation.

Tests require the explicit `test-api` feature. Synthetic V3/V4 plans and fault
injection exist only behind that feature; production selects V1 followed by V2. The
low-level connection functions are exposed only through `test_api`; default builds expose no raw connection, transaction or SQL entry point.


## Owned connections and typed requests

Creation/opening completes all compatibility checks and migrations before moving
its connection into one writer thread. Each Instance also owns one read-only
reader and watchdog by default, configurable to two readers. All connections
verify their hardening settings. Request queues default to64, configurable from
1 through256. The connection runtime uses the existing reviewed dependencies.

`Instance::store()` returns a clonable `StoreHandle` with typed metadata reads
and a health-check maintenance write. An operation deadline governs admission
to SQLite; a separate wait deadline governs how long the caller waits. Writes
use one `BEGIN IMMEDIATE` / `COMMIT`. Full queues refuse immediately. A queued
write whose operation deadline has expired never begins. Once handed off,
stopping or dropping the waiter never cancels the writer. A wait timeout is
`OutcomeUnknown`, even if the writer subsequently commits. The writer emits an
Instance-ID/operation-ID completion diagnostic and retains the latest256 true
outcomes; this memory history is not a durable idempotency ledger. IDs belong
to one open Instance runtime and are never reused within it.

`NotApplied` requires evidence: never begun, autocommit observed after a
statement error before COMMIT, or successful ROLLBACK followed by autocommit.
Domain refusals require an explicit successful ROLLBACK and autocommit;
autocommit alone is insufficient. Commit BUSY/deferred constraint need an
active transaction and confirmed rollback. Other commit errors stay `OutcomeUnknown`, even when cleanup rolls
back or autocommit is true. FULL degrades the store; I/O errors at commit,
unconfirmed rollback, corruption or writer panic fail it. Failed is terminal
for this open. Degraded refuses ordinary writes until a typed health-check
write commits for the same degradation generation. A new fault during or after
that health transaction requires a subsequent health commit; a stale success
cannot clear it. Typed command acceptance uses the same queue and failure engine.

## Durable command acceptance and startup

`CommandRequest::new(CommandId, kind, operation_version, payload)` accepts an ID
of1..128 UTF8 bytes, a kind of1..64 UTF8 bytes, version1..65535 and at most65536
payload bytes. Payloads are opaque caller-canonical bytes. The crate performs no
JSON normalization. A changed encoding needs a new operation version. Clients
persist an ID before sending and retain the identical kind, version and bytes
when retrying that ID.

The digest is SHA256 over `b"yokai.command.v1\0"`, big-endian u32 kind byte
length, UTF8 kind bytes, big-endian u32 version, big-endian u64 payload length,
then payload bytes. Command identity is excluded from the digest.

`StoreHandle::accept_command(request, operation_deadline)` returns a
`PendingCommand`; `wait(wait_deadline)` returns `Accepted`, `Duplicate` or
`Conflict`, each carrying the original `run_id`. A new acceptance commits one
Queued/Open run, one immutable command, Pending job1 and one `run.accepted`
event atomically. Matching identity and digest returns Duplicate; a different
digest returns Conflict. These decisions follow a confirmed rollback and add no
records. UPDATE, DELETE and replacement writes to existing commands are refused
by schema triggers, including REPLACE with recursive triggers disabled.

A successful acceptance reply follows COMMIT. A timeout or ambiguous result
does not cancel work; reopen if needed and retry the original request to resolve
durable truth. A duplicate reply does not mean that its run is runnable: startup
may already have placed that run on hold. Completion diagnostics retain the
typed decision when the caller stops waiting; they contain no payload bytes.

Creation initializes epoch1. Each subsequent successful open first commits an
incremented epoch and `runtime.epoch` event, then holds each Open, non-Held run
in its own transaction with one `run.held` event. RecoveryRequired takes
precedence; WaitingApproval becomes ApprovalInvalidated; other eligible runs
become StartupInterrupted. Terminal runs and existing holds remain unchanged.
Partial startup can commit an epoch and some holds before failing; reopening
continues safely without duplicating holds. Epoch overflow fails closed.
`Instance::startup_report()` exposes the completed epoch and counts. No handle
escapes before startup finishes. Successful opens therefore write durable state;
compatibility refusals retain their separate preservation rules.

V1 upgrades retain a verified pre-upgrade snapshot. V1 binaries refuse V2;
automatic downgrade and database repair are unsupported. Current validation is
native Windows MSVC. WSL processing of saved logs does not establish Linux
runtime validation, power-loss durability or product acceptance.

Reader cancellation has one guarded query ID. The watchdog interrupts only an
expired matching ID while holding the mutex used to finish that query and
start the next; it repeats every10ms across statement gaps. All statements
are finalized before the ID is cleared. Shutdown uses the same scoped path. Reader corruption also fails the store;
reader I/O faults degrade it until a health write commits.
A caller read wait timeout is distinct from the worker's `Interrupted` result.

Only the writer attempts TRUNCATE checkpoints when idle (a1s interval) and at
close. Readers disable checkpoints on close. Busy snapshots leave the
checkpoint pending. WAL metrics include a sample time and verification flag;
no hard WAL size bound is promised. Busy handling for explicit checkpoints is
nonblocking and restores the ordinary5000ms timeout afterward.

Explicit close and Drop reject new/queued requests, interrupt active readers,
finish current writer work, close every connection and join every worker
before releasing the persistent lock. Partial startup closes all connections
not transferred to workers before ownership can be released, including when
the first writer thread cannot start. The default close deadline is10s,
configurable from1ms through30s. If worker shutdown/connection close cannot be
confirmed, ownership is deliberately retained for the rest of the process:
`close` reports `CloseIncomplete`. The lock file is never removed or replaced.
Holding a cloned StoreHandle does not extend Instance ownership after close.

The failure tests distinguish real SQLite FULL/Busy/FK conditions from explicit
synthetic I/O/corruption/commit faults. Child-process reopen checks establish
observed durable truth for those cases. Windows process termination is not
Linux SIGKILL coverage. Simulated FULL does not establish physical ENOSPC;
none of these tests establishes power-loss durability or product acceptance.

## Native Windows qualification harness

The `windows_validation` custom target requires `test-api`. It exercises failed
replay settlement for Duplicate/Conflict, startup ownership after a real SQLite
close error, removed/altered V2 triggers and index in WAL/no-WAL states, compatible
V2 reopening, linked SQLite compile options, available path aliases, and a
bounded lock/contention observation. Non-Windows builds emit `RESULT NOT_CHECKED`
for this target; they do not claim Windows validation.

The bounded runner supplies a fresh absolute `YOKAI_TEST_DATA_DIR`, the explicit
Windows dumpability opt-in, and matching hash-verified V0/V1/V2 fixture and test
binary pairs through `YOKAI_TEST_V{0,1,2}_FIXTURE` and
`YOKAI_TEST_V{0,1,2}_BINARY`. Cross-version work uses copies. Original fixtures,
executables and evidence are retained. Cargo actions are offline/locked/frozen;
the test inputs must already exist locally.

`fail_startup_close_once` is diagnostic-only. A dedicated owned child leaks a
real prepared statement to make SQLite close return BUSY. The normal retention
path must keep its connection and lock until that child exits; the parent kills
and reaps only its child. Default builds expose no such hook. Replay rollback
faults are synthetic; the real transaction/ownership paths are still exercised.

Alias checks record available junction, symbolic-link and short-name cases.
Unavailable privileges or short names remain explicit gaps. Only an owned,
identity-verified link is unlinked after observation; its target and logs remain.
UNC classification is tested without opening a share. No drive mapping, share,
privilege, Defender or indexer configuration is changed. The short contention
observation does not establish controlled antivirus/indexer interference or a
performance guarantee.

These checks are partial Windows qualification. Physical NTFS exhaustion,
power loss/OS crash, directory-entry durability, Windows dumpability and
capability-dependent dangling-file-symlink coverage retain separate limits. WSL processing
of saved Windows output does not run these tests on Linux or change their scope.

### Lock sharing and open-file observations

On Windows the persistent Instance lock handle requests `FILE_SHARE_READ |
FILE_SHARE_WRITE` (mask3), omits `FILE_SHARE_DELETE`, and takes an exclusive
`File::try_lock`. Compatible raw handles can open the path; they do not become a
second owner. This is distinct from share mode zero. Held-lock rename/delete
refusal and separate synthetic share-zero/share-delete file observations are
covered by `WV-OPEN-FILES`. These experiments preserve original synthetic bytes,
measure byte-range-lock read refusal separately from compatible handle opens,
and never delete an Instance database or root.
Persistent lock bytes are compared before ownership and after confirmed close,
when an ordinary read is permitted. The observations inform future filesystem
work; they do not implement a discard operation or prove every SQLite/open-file
case on every Windows host.

R12 also attempts a dangling **file** symlink on Windows. When the existing host
capability allows it, exclusive creation must refuse without creating the
target or altering the link. The harness records the link inventory, then
unlinks only that verified owned link to leave regular sealed evidence. Missing
capability is reported as `not_checked`; no privileges or host settings change.
This new observation does not change the scope of earlier Unix-only R12 runs.
