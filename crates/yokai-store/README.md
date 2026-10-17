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

On Windows the persistent Instance lock handle requests share mode zero and
takes an exclusive `File::try_lock`. Raw read/write data handles are refused
while ownership is held. A second Instance open is classified as `Busy` on
the sharing violation. Internal entry validation requests metadata-only access
and still checks reparse attributes and the hard-link count.
`WV-OPEN-FILES` checks data-handle refusal on create and reopen and rename/delete
refusal while the reopened owner holds the lock, alongside separate synthetic
share-zero/share-delete file observations. These
experiments preserve original synthetic bytes and never delete an Instance
database or root. The earlier share-read/write implementation and its recorded
observations are superseded by this contract correction; they are not evidence
that the older code used share mode zero. Controlled Defender/indexer
interference remains untested.
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
The runtime harnesses treat only `NotFound` metadata errors as absence;
invalid paths and other I/O errors fail the observation. A dangling link itself
is present, even when its target is absent.

### SQLite open-file observations

`WV-SQLITE-FILES` uses fresh synthetic SQLite files outside Instance roots.
It checks rename/delete refusal for main/WAL/SHM files with an active WAL
writer and with a read-only transaction after the writer closes. A separate
rollback-journal fixture checks main/journal files during a write transaction
and the main file with the retained reader. The rollback mode is diagnostic;
the production store still uses WAL.

Readable bytes are retained before probes and compared afterward, alongside
file metadata. An exact Windows byte-range error33 while reading SHM is
reported as byte-unverified; metadata equality does not prove stable object
identity. Other observation errors fail the check. The harness verifies real
rollback, the literal committed row, handle release and a fresh integrity
check. SQLite's own side-file cleanup is recorded separately. These bounded
observations do not implement Instance/root discard or establish every open
file, hostile-race or durability guarantee.

### Windows preflight access refusal

`WV-PREFLIGHT-ACCESS` uses three fresh synthetic Instances with committed
future-version WALs over supported main-file headers. With all fixture SQLite
connections closed, an owned share-zero handle independently denies raw reads
of the database, WAL or SHM. Each case requires the exact Windows sharing error,
typed `PreflightUnavailable`, and no SQLite connection or fallback.

Readable baseline files are retained before denial. After releasing the guard,
the complete root listing and file bytes must match; an exclusive handle can
be reacquired, and a read-only Stage-A control must still refuse the future
WAL version without reaching Stage B. That read-only control may change SHM;
all other entries and bytes must remain unchanged. These sharing-denial cases
do not establish ACL denial, non-writable-directory behavior, or complete
PF-13/W2 acceptance. No host permissions are changed and no Instance file or
root is renamed or deleted.

### Damaged Windows WAL frames and shared memory

`WV-PREFLIGHT-DAMAGE` uses fresh synthetic future-version Instances with one
committed page-1 WAL frame. A pristine read-only control must see WAL version4
over main version3. The check then truncates a frame inside its payload, flips
one stored frame-checksum byte, or fills SHM with deterministic nonzero bytes.
For the two WAL cases, only the closed owned SHM fixture is removed before the
probe baseline so SQLite must recover its index rather than reuse stale SHM.

The WAL cases must refuse main version3; the SHM case must recover WAL version4.
Each refusal must use Stage-A read-only with no writable fallback. Every
non-SHM entry and byte must remain unchanged; SHM creation/rebuild is recorded
separately. Pristine and damaged copies are retained before the probe, and
released handles are checked afterward. These cases do not establish
supported-version startup, every corruption/recovery mode, power-loss
durability or complete PF-11/W2 acceptance. No main database, WAL or Instance
root is removed, and no host permissions change.

### Inconsistent Windows side files

`WV-PREFLIGHT-CONSISTENCY` constructs independent closed target and donor
Instances with genuine committed page-1 WALs. The target has main version3
and WAL version4; a distinct donor has WAL version5. It separately transplants
the donor WAL or SHM, or truncates a fresh closed target main file to zero
while leaving its WAL present. Pristine target/donor and altered target files
are retained before each probe. Only closed owned target SHM is removed during
foreign-WAL construction so SQLite must recover the index.

Foreign WAL must refuse version5 and foreign SHM must refuse version4 through
Stage-A read-only, without writable or raw-header fallback. Zero main with WAL
must refuse before SQLite. Every non-SHM entry and byte stays unchanged; the
zero-main case preserves the complete root. SHM effects, released handles,
retained copies and unchanged donor roots are checked separately. A foreign
SHM refusal does not by itself establish that SQLite rebuilt the SHM.

These future-version fixtures do not establish supported-version foreign-file
safety, later identity/integrity validation, all inconsistent layouts or full
PF-12/W2 acceptance. No main database, WAL or Instance root is removed or
repaired, and no host permissions change.

### Owned Windows ACL-denial fixtures

`WV-PREFLIGHT-ACL` uses three fresh closed synthetic main3/WAL4 Instances.
The test-only Windows PowerShell helper journals each original DACL before
adding a non-inheriting deny for the current SID: ReadData on the database or
SHM, or CreateFiles/CreateDirectories on the root with SHM absent. Only these
new owned targets change. No owner, SACL, privilege, execution policy or broader
host permissions change. Windows PowerShell and its Security module must already
be installed; the bounded runner pins the executable and supplies
`YOKAI_TEST_POWERSHELL`. The reviewed script is supplied over stdin without a
profile or execution-policy override.

Each raw access probe must fail with Windows access-denied5. File denials must
return `PreflightUnavailable` before SQLite; directory creation denial must
return it through Stage-A read-only. No writable or raw-header fallback is
allowed. Original permission ACE bytes and protection bits, and complete
refusal-window names/types/bytes, are restored and verified, along with retained pristine/baseline files and released
data handles. A post-restoration read-only control must still see WAL4; its SHM
creation is recorded separately. Only closed owned SHM is removed during that
fixture's construction; no main database, WAL or Instance root is removed.

RAII attempts emergency restoration. The bounded wrapper also restores scoped
journals after Cargo and after confirmed worker termination, before sealing;
rule-restoration uncertainty stops unsealed. Windows may initialize the
auto-inheritance control flag on these new fixtures; original, applied and
restored descriptors are retained. Exact full-descriptor restoration is not
claimed. A separate OS-only injected-controller
failure checks the shared cleanup path. This does not prove every kill/crash
window, Unix mode000 equivalence, arbitrary ACL/token/inheritance behavior,
hostile-race safety, supported-version startup or complete PF-13/W2 acceptance.

Stage-A read-only inspection classifies SQLite CANTOPEN and READONLY errors as
`PreflightUnavailable`, preserving their diagnostic codes. These access failures
do not establish schema inconsistency. Other inspection callers and semantic
schema/identity/integrity refusals retain their existing classifications.

### Windows Stage-B inventories and legacy control

`WV-STAGEB-REFUSALS` constructs four independent closed owned fixtures with
valid supported raw headers and no side files: a V2 foreign-key orphan, V2
metadata identity mismatch, version0 with a planted schema object, and a damaged
non-header table page. Each must reach exactly Stage-B read/write and return
the existing `StageB` envelope with its expected cause. Its `observed_effects`
must equal the measured before/after inventory. Complete pristine, altered and
post-refusal files are retained; main/identity/lock bytes, side-file existence,
exclusive data opens and a fresh child repeating the refusal are checked.
The recorded closed snapshots establish only the effects they show; they do
not prove that SQLite made no transient filesystem writes while opening.

`WV-STAGEB-LEGACY` copies the pinned accepted version0 fixture into a fresh
owned root. The existing diagnostic `open_test_versions` interface now accepts
an empty version0 plan only under `test-api`; production still selects V2.
This control requires version0, zero schema objects and no startup report,
then explicitly checks close success. Before/open/after files and inventories
are retained, including any active WAL/SHM and their later removal. Complete
closed bytes and the original source fixture must remain unchanged. Active
root names/types are checked, but the owned lock must deny reads with Windows32;
its content is compared only in the complete closed snapshots. Active retained
files exclude that unreadable lock rather than substituting its earlier bytes.

These are current shared-code observations on one Windows host. The legacy
control is not a historical-binary rerun and does not claim that normal V2
startup is non-mutating: V2 startup commits an epoch and event. Other malformed
layouts, close/crash windows, metadata/ADS/identity guarantees and complete
PF-14/PF-15/W2 or product acceptance remain separate qualification work.

### Windows Stage-A semantic and integrity refusals

`WV-PREFLIGHT-SEMANTIC` commits an FK orphan, metadata identity mismatch or
version0 planted object into a genuine WAL over an unchanged pristine V2 main
file. Each fresh owned fixture must return its exact existing typed refusal
through Stage-A read-only only. Pristine, baseline, post-refusal and child-repeat
files and inventories are retained. All non-SHM entries and bytes must remain
unchanged; SHM existence/content effects are recorded separately. Exclusive
data opens and a fresh child repeating the refusal check handle and lock release.

`WV-PREFLIGHT-INTEGRITY` changes only the fragmented-free-byte count of an
empty non-header runs-table leaf from0 to1. The exact integrity-result row must
be returned through the existing StageB envelope with no WAL, and as typed
`Integrity` through Stage-A read-only with an empty WAL. Both paths retain
complete copies/inventories and verify released handles and child ownership.
The raw-header path requires complete closed-byte equality and measured effects;
the read-only path permits only the recorded SHM differences.

These are current-code synthetic Windows observations. They do not establish
every malformed page, all PF6/PF7/PF8/PF14/W2 requirements, absence of transient
writes, nonempty StageB difference formatting, power-loss durability, broader
security or product acceptance. An integrity-result row is not an extended
SQLite error code. No Instance main/WAL/root is removed or repaired.

### Measured Windows resource counters

IL-8 now samples real native process handle counts and PID-filtered Toolhelp
thread snapshots in both parent and child. Each process completes one explicit
open/close warmup, then100 measured cycles, alternating between processes.
Bookend counts must be positive and equal after confirmed close and owned
process/pipe cleanup. Counter, enumeration, protocol and close errors fail.
Unix `/proc` counter errors also fail; these edits have no new Linux runtime
qualification.

`WV-RESOURCE-COUNTS` checks32 repeated samples, null-query native error6,
eight held ordinary file handles, four ready blocked threads, and return to
baseline after join/close. Synthetic files are retained outside Instance roots.
The test-only shared helper counts handles before opening its snapshot, closes
that snapshot, initializes the28-byte native entry, checks its returned length,
and accepts only ERROR_NO_MORE_FILES after Thread32Next as normal completion.
No dependency, privilege or host setting is added.

Older Windows IL-8 runs used placeholder zero counters: their200 completed
cycles remain historical evidence, but their resource-stability text was
unsupported. New counts are non-atomic samples on one host. They do not establish
absence of transient leaks, all-time resource stability, other platform/host
behavior, performance bounds, full IL-8/W2, or product/release acceptance.

### Windows corruption with a genuine committed WAL

`WV-PREFLIGHT-CORRUPTION` retains three independent closed V2 fixtures and
commits a literal health marker into a genuine WAL without checkpointing.
A read-only control must see that marker, version2 and integrity `ok` while
leaving main/WAL bytes unchanged. WAL frames must affect only the metadata
page, excluding page1 and the runs leaf, so replay cannot mask the main fault.

The cases overwrite exactly16 magic bytes, change one empty non-header runs
leaf fragmentation byte from0 to1, or truncate the13-page main to exactly half
its length. The pinned fixture uses4096-byte pages. Exact `HeaderInvalid`
magic/length refusals must happen before SQLite; the leaf must return the
exact collected free-space `Integrity` result row through Stage-A read-only.
Generic access/store errors or Stage-B fallback fail these observations.

Pristine, healthy-WAL, damaged, post-refusal and child-repeat copies/inventories
are retained. Main/WAL/id/lock and all protected names/types/bytes must remain
unchanged; only the leaf's read-only SHM effects are allowed and recorded.
Exclusive data opens and a fresh child repeating each exact refusal check
released data handles and Instance ownership. No main/WAL/root is deleted or
repaired. The historical broad PF6 and empty-WAL observations retain their
original scope. These fixtures do not establish every corruption/geometry,
absence of transient writes, file identity/metadata/race safety, full PF6/W2,
power-loss durability, security or product acceptance. An Integrity result row
is not a SQLite extended error code.

### Windows raw-header refusal witnesses

`WV-RAW-HEADER` uses fifteen independent closed V2 roots without side files.
Nine cases alter magic, page-size encoding1000/65535, format3/3, reserved8,
payload fractions65/32/32, schema format5, text encoding2 or file length minus1.
Each also contains max unsigned user_version; the exact `HeaderInvalid` field
must take precedence over a newer-version refusal, with no SQLite connection.
SQLite has a16-bit page-size field:1 denotes65536;131072 cannot be directly
encoded. The actual malformed upper encoding tested here is65535.

Six otherwise unchanged headers encode future versions3,255,256,65536,
16777216 and4294967295. Each must return exact `SchemaTooNew` found/maximum2
values before SQLite. Accepted0/1/2 behavior retains its separate migration
and startup coverage. Connection hooks establish the observed route; they
are not instrumentation of every internal version-decode operation.

Pristine, altered, post-refusal and fresh-child copies retain180 ordinary files.
Complete closed names/types/bytes and explicit absent WAL/SHM/journal entries
must match after refusal and child repetition; exclusive data opens check
released handles. No main/WAL/root is deleted or repaired. These selected
encodings do not establish exhaustive valid headers, file identity/metadata,
absence of transient writes, hostile-race safety, full PF16/PF17/W2, Linux
parity, power-loss durability or broader product/security/release acceptance.

### Validated raw-header decoding observations

The explicit `test-api` feature now records the unsigned version immediately
after all existing raw-header guards and the big-endian decode. Its fallible
`header_versions` endpoint takes and clears the recorded values; `clear` resets
them. The process-wide buffer is intended for serialized owned probes.
A poisoned observation buffer reports an error, rather than a successful
empty observation. Recording failure becomes diagnostic `PreflightUnavailable`.
Default builds contain neither the observer nor its public diagnostic endpoint;
the default header acceptance rules and schema remain unchanged. The empty-file
shortcut is not a decoded-header event.

PF17 retains 54 ordinary main/identity/lock input and output files for versions
0/1/2/3/255/256/65536/16777216/4294967295. Every input is a nonempty header, and
exactly one matching observation is required, followed by an empty second take.
Accepted 0/1/2 opens reach Stage B and finish at V2 after migration/startup and
explicit close. Those opens intentionally change the main database. Future
versions refuse before SQLite and preserve the complete closed root. Fixed-name
SQLite inventories and unchanged identity/empty-lock bytes are checked separately;
the accepted roots' backup contents are not requalified by these copies.

`WV-RAW-HEADER` requires no observation for each of its nine malformed headers,
and one exact observation for each of its six future versions, in both parent
and fresh child. The unwired observer failed this predicate before recording
was implemented; that diagnostic failure is retained separately from acceptance.
These are source-bound hook observations, not independent instruction/syscall
traces or proof of every internal operation. Literal 131072 page-size clarification,
exhaustive headers, other platforms, concurrency/poisoning failure experiments,
transient writes, metadata/object identity, full catalogue/workspace/security,
power-loss durability and release acceptance remain separate work.
