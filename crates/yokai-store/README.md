# Instance schema migrations

The current schema version is **1**. It adds only the STRICT singleton
`instance_meta` table and the owned runner's `yokai_schema_history` table.
Commands, Runs, jobs, workers and dispatch are outside this slice.

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
low-level connection API remains until the following connection-ownership slice.
