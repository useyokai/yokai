# Instance schema migrations

The current schema version is **1**. It adds only the STRICT singleton
`instance_meta` table and the owned runner's `yokai_schema_history` table.
Commands, Runs, jobs, product workers and dispatch remain outside this crate API.

SQL lives in `migrations/V{n}__{name}.sql` and is embedded at compile time.
Each migration ends with its matching `PRAGMA user_version = n;`. The
conservative lint rejects transaction control, connection-changing statements
and quoted identifiers. Migration SQL, its history row, version and the V1
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

Tests require the explicit `test-api` feature. Synthetic V2/V3 plans and fault
injection exist only behind that feature; production always selects V1. The
low-level connection functions are exposed only through `test_api`; default builds expose no raw connection, transaction or SQL entry point.


## Owned connections and typed requests

Creation/opening completes all compatibility checks and migrations before moving
its connection into one writer thread. Each Instance also owns one read-only
reader and watchdog by default, configurable to two readers. All connections
verify their hardening settings. Request queues default to64, configurable from
1 through256. No dependency or schema version is added by this runtime.

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
cannot clear it. Application command identity/domain operations are a later API.

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
