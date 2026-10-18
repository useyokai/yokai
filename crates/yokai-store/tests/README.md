# Storage validation harnesses

This guide describes the custom harnesses configured in
[`Cargo.toml`](../Cargo.toml). They all require `test-api`; each prints named
`CHECK` records and a final `RESULT`. Inspect both the process exit and recorded
results. An unselected or platform-inapplicable check is not a pass.

## Inputs and recorded scope

The native Windows/MSVC suite includes the following configured targets.
Recorded Windows runs use fixed NTFS:

| Target | CHECK records | Additional inputs beyond the synthetic data directory |
| --- | ---: | --- |
| `durable_primitives` | 21 | `YOKAI_TEST_V1_FIXTURE`, `YOKAI_TEST_V1_BINARY` |
| `instance_lifecycle` | 46 | None |
| `instance_migrations` | 18 | `YOKAI_TEST_V0_FIXTURE`, `YOKAI_TEST_V0_BINARY` |
| `sqlite_runtime` | 12 | None |
| `store_runtime` | 31 | None |
| `windows_validation` | 25 | `YOKAI_TEST_V0_FIXTURE`, `YOKAI_TEST_V1_FIXTURE`, `YOKAI_TEST_V2_FIXTURE`, `YOKAI_TEST_V2_BINARY`, `YOKAI_TEST_POWERSHELL` |

These 153 records include explicitly qualified platform branches. R12 attempts
the dangling-file-symlink subcheck on Windows and records `checked_windows`
when the host supports it, or `not_checked` when that capability is unavailable.
Earlier runs retain their original limits. The Windows-only target reports
`RESULT NOT_CHECKED` on other platforms.
The checkpoint's WSL gate parsed saved Windows output; it ran no Linux candidate
tests. Counts describe this checkpoint, not a complete acceptance catalogue.

`YOKAI_TEST_DATA_DIR` must name an existing absolute directory reserved for
synthetic test data. The other fixture variables name preserved, closed owned
Instances; binary variables name the matching pinned historical executables.
Record hashes and source identity before use. The historical artifact package
and complete bounded runner are not distributed in this repository, so a clean
clone alone cannot reproduce the full checkpoint. Never substitute a live
Instance, an arbitrary old binary, or a newly generated fixture and call it the
same historical compatibility check.

The Windows ACL cases temporarily change permissions on newly owned fixtures.
They require Windows PowerShell and its Security module, with the selected
executable supplied through `YOKAI_TEST_POWERSHELL`. A full-suite runner must
retain ACL journals, confirm child termination, restore and verify the scoped
rules, and preserve a failed attempt. Stop when restoration is uncertain.
These requirements go beyond a bare `cargo test --workspace` invocation.

See [CONTRIBUTING.md](../../../CONTRIBUTING.md) for toolchain/cache requirements,
offline flags, the Windows dumpability opt-in, and a self-contained SQLite
invocation. Keep test data, cache, and build outputs separate by platform.
Directory-entry/power-loss durability, physical disk-full behavior, exhaustive
Windows file semantics, and full product/security acceptance remain unproven.

## Native Windows qualification coverage


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
and fresh child. These are source-bound hook observations, not independent instruction/syscall
traces or proof of every internal operation. Literal 131072 page-size clarification,
exhaustive headers, other platforms, concurrency/poisoning failure experiments,
transient writes, metadata/object identity, full catalogue/workspace/security,
power-loss durability and release acceptance remain separate work.

### Windows journal and zero-length database refusal witnesses

`WV-JOURNAL-EMPTY` retains nine owned fixtures: rollback format bytes1/1;
journal presence with no WAL and with a genuine committed WAL; and zero-length
main with empty or nonempty WAL, SHM or journal. Refusals must match the exact
existing typed errors before any SQLite connection. Valid main headers are
decoded before journal presence is checked: max unsigned version without WAL,
and main2 with committed WAL4. Rollback headers and empty-main shortcuts produce
no decoded-header observation. Every observation drains on read.

Pristine, altered, post-refusal and fresh-child files retain140 ordinary files,
including empty side files as present entries. Parent and child repeat the same
strict oracle, compare complete closed roots and all four fixed SQLite inventory
entries, and verify released data handles. This distinguishes empty side files
from absent ones without repairing or deleting any Instance main, WAL or root.
Journal payloads are synthetic presence witnesses, not crash-produced hot-journal
recovery qualification.

These selected PF4/PF5/PF18 observations do not prove transient-write absence,
metadata/ADS/object identity, hostile-race safety, every side-file combination,
Linux parity, full W2/workspace/security/release or power-loss durability.

### Stale SHM without WAL

`WV-STALE-SHM` uses ten independent closed owned V2 fixtures. Each has a
validated main header at version0,1,2,3 or4294967295 and an empty or nonempty
SHM, with WAL and journal absent. Exact `SideFileStateUnverified` with that
unsigned header version must take precedence over newer-version and schema
work, with one decoded-header observation, a drained second take, and no
SQLite connection. The legacy header values are synthetic; they do not
establish a legacy schema upgrade.

Pristine, altered, parent-after and fresh-child-after copies retain150 ordinary
files. Both processes enforce the same exact refusal, complete closed-root
byte equality, fixed SQLite entry inventories and released data handles.
Empty-present SHM is distinct from absence. These selected PF2b/PF10(a) checks
do not establish supported-version side-file recovery, every layout, transient
writes, metadata/ADS/object identity, hostile races, Linux parity, full W2,
power-loss durability or product/security/release acceptance.

### Empty WAL versus absent WAL

`WV-EMPTY-WAL` pairs six independent closed V2 fixtures at future header
versions 3, 16909060 (0x01020304) and 2147483647. Each has an empty or absent
WAL, with SHM and journal absent at setup. An empty WAL requires exactly one
`stage-a-read-only` connection; an absent WAL requires none. Both paths must
return exact `SchemaTooNew` with the observed version and maximum 2, record
one decoded header value and drain the observation on a second take. A fresh
bounded child independently repeats each oracle.

Complete pristine, altered, parent-after and child-after snapshots are retained
and compared with all fixed SQLite inventories. An empty WAL must remain
present and zero bytes; all non-SHM entries stay byte-identical. SHM effects
are measured and retained. Absent-WAL complete roots stay byte-identical.
Exclusive data opens and child lock reacquisition check released handles.

These selected PF10(b) and future-header PF2a/PF10(c) witnesses do not qualify
legacy or supported-version recovery, actual future schemas, header values
above SQLite's signed PRAGMA range, every layout, transient writes,
metadata/ADS/object identity, hostile races, Linux parity, full W2,
power-loss durability or product/security/release acceptance.

### Supported opens with all side files absent

`WV-SUPPORTED-OPEN` copies the three ordinary files from pinned genuine V0,
V1 and V2 fixtures into separate owned roots. It preserves the originals and
uses production `open_instance`, with no altered header values or diagnostic
migration plans. Each raw-header path records its original version once,
drains the observation, and proceeds through exactly one Stage-B read/write
connection, with no Stage-A read-only event. Close records connection release
before lock release.

V0 and V1 upgrade to V2 and retain their two and one pre-migration backups;
current V2 creates none. Startup reports epoch 2 with no examined or changed
runs. A fresh bounded child opens the resulting V2 root at epoch 3 without
another migration. All closed states have WAL, SHM and journal absent.

Before, parent-after and child-after snapshots retain 33 ordinary files,
including every backup, and the backup-directory layout. Identity/lock bytes,
source fixtures and child-reopen backup bytes remain equal; main database
changes are expected and measured. Exclusive opens check released handles for
data, backups and lock files. These are selected PF10(c) and upgrade witnesses,
not a non-mutating startup claim or full PF10/W2, historical-binary, transient
write, metadata/ADS/object-identity, hostile-race, power-loss/OS-crash, Linux
runtime or product/security/release acceptance.

### Supported opens with an empty WAL

`WV-SUPPORTED-WAL` copies genuine pinned V0, V1 and V2 fixtures into separate
owned roots and adds a zero-byte ordinary WAL, with SHM and journal absent.
The original main, identity and lock bytes are preserved without header edits.
Production `open_instance` records the original header once and drains the
observation, then records Stage-A read-only followed by Stage-B read/write.
The production Stage-A inventory guard remains active; retained snapshots
cover the whole open/close boundaries rather than a separate intermediate
Stage-A capture. Close records connection release before lock release.

V0 and V1 upgrade to V2 and retain two and one pre-migration backups; V2
creates none. All report startup epoch 2 with no examined or changed runs.
A fresh bounded child reopens the closed current-V2 root at epoch 3 through
the raw-header/Stage-B path, preserving migration history and backup bytes.
All final closed states have WAL, SHM and journal absent.

Before, parent-after and child-after snapshots retain 36 ordinary files,
including each initially empty WAL and all backups, plus backup-directory
markers. Source, identity and lock bytes remain equal; main changes and WAL
removal during successful writable startup/close are expected. Exclusive
opens check released data, identity, backup and lock handles. These selected
supported PF10(b) and upgrade observations do not establish full PF10/W2,
non-mutating startup, transient-write absence, metadata/object identity,
hostile-race safety, crash recovery, power-loss durability, Linux runtime or
product/security/release acceptance.

### Creation, close and forward-snapshot checks

The lifecycle target keeps the existing CHECK IDs while expanding their
subcases. IC-6 covers five creation boundaries, both exit and parent-kill
modes, and three rounds of each. IC-9 reopens those same 30 incomplete roots
five further times each. Every typed refusal compares a retained inventory
of ordinary-file names, types, sizes and SHA-256 values. Inventories live
beside each root; unexpected entry kinds and metadata/read errors fail the
observation. The incomplete roots are not repaired or removed.

IL-7 pauses an owned child's close at shutdown before worker joins and after
successful joins with connection closure confirmed, before ownership release.
Another process must observe `Busy` at both boundaries. An explicit stdin
command resumes the child, which verifies the existing aggregate
`connection-closed`, then `lock-released` events; a fresh open must succeed.
The post-join marker confirms completed shutdown. Worker-owned connections
physically close during worker shutdown, before those joins complete.
The pause points and bounded resume command exist only with `test-api`.

IL-10 compares actual persistent lock file identity and size zero after
orderly close, owner exit, and both explicit CloseIncomplete and Drop with an
unjoinable reader followed by parent kill/reap. The latter cases also require
a contender's `Busy` while the owner lives and an integrity-checked reopen
after death. PF-3a/b now kill and reap a live SQLite WAL producer after its
committed-version readiness marker; the original main-header version must
remain visible while the future version is read through the retained WAL.

MG-7 compares every typed schema/table value in the new pre-V2 snapshot with
the retained prior database, using canonical row order. Two distinct command
payloads provide nonempty data; a same-cardinality changed-value comparison
must differ. Two earlier backups remain byte-identical, command values
survive the forward test-only V3 migration, and logical images, the prior
main file and backup hashes are retained outside the Instance root.

These are synthetic native Windows witnesses. Parent kill uses Windows
process termination; it does not establish Linux runtime or power-loss
behavior. The checks add no directory-flush, physical NTFS-full, hostile-race,
full Windows qualification, security or product acceptance claim.


### Owned junction paths

WV-ALIAS retains its earlier cases and adds directory junctions at each of
the six required Instance entry names: lock, identity, database, WAL, SHM and
journal. Parent and separate child must return `PathInvalid` before any SQLite
connection event. Any original ordinary file is retained outside the root.
Complete before/after inventories record ordinary files and directory reparses
without following links; a separate target inventory protects outside bytes.
An equal-length changed-content control must produce a different inventory.

A direct junction root is also refused as `PathInvalid`. A junction in an
ancestor of an ordinary Instance leaf is allowed through canonicalization:
an unowned open binds the real root, parent and child contenders are `Busy`
while the real owner holds it, and a fresh child reopen after close succeeds
with integrity intact. Only verified owned reparse entries are unlinked;
targets, original bytes, inventories and child logs are retained.

These owned-fixture checks do not qualify hostile reparse races. They create
no drive mappings or shares and enable no host features. Distinct 8.3, subst,
actual accessible UNC/mapped-network and removable-volume cases still need
their own recorded host capabilities and scoped cleanup. Syntax-only UNC or
fixed-drive success does not satisfy a negative-volume criterion. The
operator-provided host check below adds those actual observations; unavailable
mandatory host cases keep B3 and full Windows qualification open.

### Operator-provided host paths and negative volumes

WV-HOST-PATHS requires an explicitly bounded Windows operator wrapper. Missing
inputs fail the check; a USB label or syntax-only UNC string cannot pass it.
The operator owns fresh short-name, subst, actual accessible UNC/mapped share
and removable-NTFS fixtures, pins their identities and keeps them available
through the batch. The harness creates no mapping or share and changes no
host filesystem feature. Host paths and private execution records stay outside
this repository.

Distinct short-name and subst paths must bind the same native file identity.
Both owner directions require parent and child `Busy` before SQLite events,
unchanged protected bytes/lock identity, then fresh-child reopen and integrity.
The persistent lock identity spans owner close and child reopen; the observed
startup report and persisted epoch must each advance by exactly one per open.
UNC, mapped remote and actual `DRIVE_REMOVABLE` paths each require parent and
child create/open `UnsupportedLocation` before SQLite, complete refusal
inventory equality (including native file IDs) and released lock handles.
The removable fixture is a raw copy of a closed synthetic fixed-drive Instance;
the application is never used to create a successful Instance on that medium.

The owned C:/removable fixture budget is 64 MiB, 128 ordinary files and 16 MiB
per file, checked without following reparses. Wrapper monitoring is sampled,
not OS confinement or a quota. All fixtures and failures remain available.
The added check makes the catalogue 154. Results apply only to the observed
source/tool/host binding; hostile alias races, full Windows qualification,
directory-entry and power-loss durability, full-disk qualification and
promotion remain separate.
The Windows harness stops at its first failed mandatory check and retains the
failure output; later checks do not create more fixtures after that failure.

### Controlled Defender and indexer profile

`windows_validation --only WV-INTERFERENCE` is a separate operator profile.
It requires `YOKAI_B4_FIXTURE_ROOT`, `YOKAI_B4_CONTROL_ROOT` and a fresh
alphanumeric `YOKAI_B4_NONCE` from a bounded reviewed native wrapper. The
ordinary 154-check catalogue is unchanged; this additional profile must be
included when qualifying controlled interference. A running service or the
natural lock soak cannot satisfy it.

Four synchronized phases hold a real uncommitted command/owner, pause close
before worker joins and after confirmed joins but before unlock, and hold an
acknowledged writer before parent kill/reap and fresh-process recovery.
Each phase checks parent/fresh-child `Busy` before SQLite. The oracle checks
ordered close/release events, persistent lock identity, exact reopen epochs,
the complete 26-command ID/kind/version/payload set, dependent record counts,
absence of the killed uncommitted command, integrity and foreign keys.
Minimum held intervals are 15/10/10/15 seconds, each bounded at 120 seconds.

The wrapper must separately prove actual scoped Defender scan callbacks and
fresh unique-content indexing observations inside each held interval. Control
release markers only coordinate the storage harness; they are not independent
proof of service activity. Pin tool versions/hashes, source, command scope,
request counts, bytes, time/resource limits and final evidence custody. After
fresh recovery and all closed-state reads, the harness emits a complete ordinary
entry inventory with file hashes, sizes and native identities. The wrapper must
match it, retain closed bytes in its sealed package, and recheck the original
fixture before acceptance. Check free space on both fixture and evidence volumes;
these controls are sampled and do not provide a filesystem quota.
The harness changes no security/indexing settings and creates no service or
mapping. Existing services are outside the test Job; unknown completion remains
failure with fixtures retained. No global scan cancellation is implied.
Neither this profile nor its source CHECK alone establishes full Windows,
directory-entry/power-loss durability, hostile-race freedom or promotion.
