# Headless conversation core

The conversation runtime can accept a message, stream a deterministic reply,
commit the result and replay it after an ordinary close and reopen. The trusted
Rust caller owns the Instance and supplies command, conversation and message IDs.

## Try it with synthetic data

Use the Windows toolchain in [Contributing](../CONTRIBUTING.md). Create an empty
directory for synthetic data outside the checkout. Pass its absolute path as
$PARENT. The example creates a fresh child directory and retains it for inspection.

~~~powershell
cargo run -p yokai-exec --example conversation_core --target x86_64-pc-windows-msvc --offline --locked --frozen -j 4 -- --parent $PARENT --message 'hello'
~~~

The example checks ordered progress, a confirmed completion and matching
message IDs and bodies after reopen. Repeating the command resolves to the
same completed run. A successful example prints `CHECK M1-DEMO PASS`.

## Requests and progress

`CreateConversation` binds a command ID to a conversation ID. `SubmitMessage`
adds a caller-chosen user message ID and 1 to 16,384 UTF-8 bytes. Its version 1
payload is the conversation ID, user message ID, a big-endian 32-bit byte count
and the exact text bytes. The existing command digest binds the kind and version.

The Instance writer looks up the command identity before checking capacity.
An exact repeat returns the original run and its recorded state. A different
payload with that ID returns `Conflict`. A different submission can start only
when its conversation has no active run and fewer than four fake runs are
admitted in the current epoch. `Busy` leaves the command available for a later
submission with the same ID.

The runtime starts the fake after confirmed durable acceptance. The reply is
the literal prefix fake: followed by a space and the exact input text. The
response limit is 65,536 bytes. The runtime sends numbered chunks of at most
1,024 bytes at UTF-8 boundaries through a channel with room for 16 items.
The client supplies an assistant message ID for a new run. When the progress
channel is full, the fake gives the reader up to 250 ms to drain it, bounded by
the operation deadline. The fake stops if the channel stays full or the reader
disconnects. The run becomes failed when the failure transaction commits.
An adapter failure also leaves no committed assistant message. Read progress
while the fake runs and wait separately for the terminal decision.

The writer reserves four queue slots for terminal writes, so ordinary queue
saturation cannot refuse a worker's completion. It commits the final message,
run and job state, slot release and completion event together. A store error
retains its `NotApplied` or `OutcomeUnknown` evidence. A caller wait timeout
leaves a handed-off operation eligible to finish. Resolve ambiguity through the original command identity
and replay. A duplicate never starts another fake. If durable acceptance
succeeds and dispatch cannot start, the open run remains available for
inspection and becomes `Held` on reopen.

## Reopen and replay

V3 adds conversation and message bindings to the existing records. A V2
upgrade retains a verified pre-upgrade snapshot and the V1/V2 history.
Reopen places an interrupted open run on hold. It blocks new work on that
conversation and requires a future resume or abandon operation.

`read_conversation` returns the latest run state. Use `replay_events` to
reconstruct the conversation in event sequence order, including the
committed user and assistant messages. Replay starts with a cursor containing
the Instance and conversation IDs. Continue from each returned cursor while
`has_more` is true. Pages allow 1 to 256 events and at most 256 KiB, accounting
for UTF-8 bodies and fixed event fields. Foreign, future and out-of-stream
cursors fail with `InvalidCursor`. Progress chunks live only in the stream.

The focused Windows checks use disposable SQLite fixtures and deterministic
faults. The interruption check covers logical recovery after ordinary close
and reopen. Real providers, authenticated IPC, desktop interaction, hostile
paths, physical disk exhaustion and power-loss durability need separate work.
