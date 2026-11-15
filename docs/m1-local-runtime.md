# Local runtime connection

The Windows runtime owns one Instance and exposes the synthetic conversation
service through a local named pipe. It reuses the existing writer and fake
adapter. A separate native client creates conversations, submits messages,
observes provisional progress and replays committed results after reconnect or
explicit reopen. This is a development slice, not a desktop or release-ready app.

## Enrollment and trust

Both peers must be ordinary, non-elevated interactive processes with matching
Windows user, logon SID, session ID and integrity level. Enrollment contains
`peer_image`, `peer_sha256` and `instance`: the absolute expected executable path,
its lowercase SHA-256 and the expected Instance ID. Trusted setup supplies and
protects these records. The runtime permits an empty Instance only during first
creation; the client requires a known specific ID. Never take trusted executable
pins from an untrusted locator or peer-supplied JSON.

Names use a fixed local prefix and validated nonce. The server rejects remote
clients, uses first-instance protection and verifies its protected logon-SID DACL.
The client opens with asynchronous I/O and
`SECURITY_SQOS_PRESENT | SECURITY_IDENTIFICATION`. No server impersonation is used.
Each side queries the OS pipe peer PID, process identity and enrolled image;
the client also checks expected server creation time. Instance, runtime epoch,
session and sequence bind subsequent frames without supplying caller authority.

An ACL alone is not executable identity. These checks assume trusted launches,
enrollment and binaries in the same account. They do not protect against account
compromise, code injection, a compromised enrolled client, or replacement of
trusted setup. The executable hash is not loaded-module attestation. Authenticated
clients have the same supported access to this Instance, including shutdown;
there is no per-client role or multi-user authorization service. Control records
contain no provider secrets.

## Start and discover

Start explicitly with `yokai-runtime create --parent <absolute-fixture-parent>`
or `yokai-runtime open --instance <absolute-instance-root>`, plus
`--control <absolute-control-directory> --peer-config <absolute-enrollment-file>`.
Parent/control directories must already exist and belong to trusted setup.
Use fresh synthetic fixtures and preserve them on failure.

The runtime opens or creates the Instance before endpoint publication. Failed
startup does not promise that no database/epoch change occurred. A second owner
is refused by the Instance lock before migration or discovery publication.
The control directory holds `endpoint.json` and one saved record per published
epoch. Discovery supplies addressing, not trust. Match it against the known
Instance and enrollment. Locators can remain stale after close or failed startup.
Connection refusal does not authorize deleting the lock or attaching to another
owner. Explicit reopen takes the lock and publishes a fresh epoch. Control-file
history retention is not automated.

## Requests and durable results

Invoke `native_client --endpoint <locator> --peer-config <client-enrollment>`
with `status`, `create`, `submit`, `replay` or `shutdown`. Create, submit and replay
read the corresponding typed JSON from stdin. The Rust `Remote` API additionally
exposes the conversation read request. The native example prints one JSON reply
per line. Inspect its decision/error fields: exit code zero can also mean a
protocol refusal was received. Submit's `accepted` is followed by progress and
a terminal reply.

Keep the original command ID, message ID and payload to resolve uncertain
acceptance. Exact duplicates resolve the original run; changed data conflicts.
Reconnect/replay with the original Instance, conversation and committed cursor.
Invalid or future cursors are refused. Progress is provisional; only writer
confirmation or durable replay establishes completed output.

The runtime drains admitted work after client exit or subscriber detachment.
Disconnection or an expired wait does not imply rollback, stopped work or
permission to dispatch a competing request. Store errors preserve
`not_applied`/`outcome_unknown` certainty and phase. Transport loss, completion
wait expiry and actual uncertain commit are distinct. After framing/I/O failure,
discard the connection and reconcile on a new authenticated connection; do not
resume a partly consumed byte frame.

Frame limits are 4 KiB for controls/handshake, 128 KiB for requests and 2 MiB for
responses. User messages are at most 16,384 UTF-8 bytes; core responses at most
65,536 bytes. Replay is bounded by event count and payload before encoding.
There is one queued ingress request, 16 progress notices and one terminal notice
per session, and at most four active fake runs. Overflow detaches the subscriber;
durable results remain replayable. These are application bounds, not global
memory or filesystem quotas.

## Shutdown and ownership

Shutdown permanently closes mutation admission and drains accepted work before
store close. `ShutdownAccepted`, EOF or a returned launcher prompt does not prove
runtime exit. Observe owner Closed and runtime process exit separately when
confirmed lifecycle closure is required.

A known pre-store drain that misses its deadline retains the unconsumed Instance
and reports `close_incomplete`. Once the old reader has joined and pipe completion
is known, a new authenticated connection may query Status or explicitly request
Shutdown again. Mutation admission stays closed. There is no automatic retry,
restart policy or supervisor. Uncertain native I/O, an unjoined reader or failure
after consuming the Instance retains the responsible owner; it does not authorize
another close or a fabricated Closed result.

An observed progress-relay panic retains the Instance owner. During an active
session the runtime preserves the failure through ingress reader cleanup and
reports CloseIncomplete before it can accept another session or report Closed.
Ordinary transport disconnects continue through the reconnect path.

## Validation boundary

The earlier native Windows matrix and adapter suite support paths whose inputs
are unchanged. The relay correction passed seven focused normal cases, including
14 relay and ingress unit tests and default and release reopen flows. A separate
synthetic fault in the active session confirmed CloseIncomplete, retention of the
owner and refusal of a second owner before store access. Its bounded test Job
termination was recorded separately from graceful shutdown.

The foreign-SID check uses synthetic native Authz evaluation. Frame high-water
measures an individual owned read buffer. General storage quiescence, hostile
same-account protection, power-loss durability, desktop and supervisor lifecycle,
credentials and full M1 acceptance remain outside this evidence.
See [testing and fixture requirements](m1-local-runtime-testing.md).
