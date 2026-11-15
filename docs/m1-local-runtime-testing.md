# Local runtime build and fixture guide

This describes current source interfaces and saved validation scope. These are
future developer procedures; this documentation revision ran none of them.
They do not recreate a qualification run's process limits or custody records.
Use only fresh synthetic data and preserve failures.

## Compile before runtime validation

Use the pinned native Windows/MSVC environment in [Contributing](../CONTRIBUTING.md).
Assign each profile a separate fresh target directory outside source/live data.
Keep Cargo caches and temporary files separate from runtime fixtures. The
offline prerequisites must already exist; do not regenerate the dependency lock
or silently fetch missing packages.

These command forms compile only. Change the absolute `$BuildTarget` between
profiles and preserve Cargo's JSON output and exit status:

```powershell
cargo test --workspace --all-targets --no-run --features yokai-store/test-api,yokai-exec/test-api,yokai-platform/test-api,yokai-ipc/test-api --target x86_64-pc-windows-msvc --target-dir $BuildTarget --offline --locked --frozen -j 4 --message-format=json-render-diagnostics
cargo build --workspace --bins --examples --target x86_64-pc-windows-msvc --target-dir $BuildTarget --offline --locked --frozen -j 4 --message-format=json-render-diagnostics
cargo build --workspace --bins --examples --release --target x86_64-pc-windows-msvc --target-dir $BuildTarget --offline --locked --frozen -j 4 --message-format=json-render-diagnostics
```

Select executables from `compiler-artifact` records using package, target name/kind,
features, profile and target triple. Record absolute path and SHA-256 alongside
source and lock hashes. Do not select by glob or silently reuse an old binary.
Runtime validation invokes that recorded executable directly, without Cargo.
For standard Rust test binaries, use exact names, `--show-output` and
`--test-threads=1`, checking both named results and filtered counts. Custom
`harness = false` targets instead require their documented arguments.

| Target | Requirements |
| --- | --- |
| IPC `protocol` | Standard tests; `test-api`; seven current named tests |
| Platform `local_ipc_windows` | Standard Windows tests; `test-api`; six named tests; creates pipes and queries identity/security |
| Exec library tests | Ten Windows ingress and four relay unit tests |
| Exec `local_runtime_windows` | Custom helper; explicit mode, locator and enrollment; external runtime/fixture owner |
| `yokai-runtime` / `native_client` | Separate processes; use no-feature default/release binaries for product-path checks |
| Conversation/store targets | Custom CLI; consult the [storage harness guide](../crates/yokai-store/tests/README.md); some cases create children or require historical fixtures |

Do not launch every test merely because all targets compiled.

## Trusted fixture setup

Use a non-elevated interactive Windows context. Establish runtime process ownership
before launch: client exit must not end the runtime through inherited handles or
a parent Job policy. This guide supplies no breakaway, termination or cleanup
authority. Preserve uncertain native completion or ownership for explicit recovery.

Prepare a fresh fixed-NTFS fixture outside source, with existing separate `data`,
`control`, `tmp` and log directories. Trusted setup owns enrollment and control
files; verify permissions and path/reparse ownership before use. Do not reuse live
data. Record the selected runtime and client executable hashes.

The runtime enrollment is bounded UTF-8 JSON without a BOM. Supply the actual
approved client path/hash, allowing an empty Instance only during first creation:

```json
{"peer_image":"C:\\approved-build\\native_client.exe","peer_sha256":"<64 lowercase hex characters>","instance":""}
```

These placeholders are not runnable enrollment. Start the recorded runtime with
the `create` argument form in the [runtime guide](m1-local-runtime.md). Record its
PID/creation identity and announced Instance root inside the fixture. Compare
the locator's owner and Instance against that trusted creation record. The root's
last component is the 32-digit hexadecimal Instance ID; protocol IDs use its
hyphenated display form. Do not inspect the live SQLite database for identity.

Create the client enrollment with the approved runtime path/hash and that known
Instance ID. Preserve both records. Client syntax is:

```text
native_client --endpoint <absolute-locator> --peer-config <absolute-client-enrollment> <status|create|submit|replay|shutdown>
```

Create, submit and replay consume one matching UTF-8 JSON body on stdin, then EOF.
The following illustrate three separate invocations on a fresh conversation:

```json
{"op":"create","command":"demo-create","conversation":"00000000-0000-4000-8000-000000000001"}
{"op":"submit","command":"demo-submit","conversation":"00000000-0000-4000-8000-000000000001","message":"00000000-0000-4000-8000-000000000002","text":"hello"}
{"op":"replay","instance":"<trusted Instance ID>","conversation":"00000000-0000-4000-8000-000000000001","after":0,"limit":20}
```

Do not send these as one stdin stream. Parse each reply's decision/error fields;
process exit zero alone does not mean the request succeeded. Require confirmed
completion and exact replayed message IDs/text. Resolve a lost reply with the
original command/payload. For reopen, first confirm runtime close and process
exit, then use `open --instance` with the same data and trusted configuration.
Expect a fresh epoch. Never delete the Instance lock to make reopening succeed.

## Cross-process helpers and controls

The helper takes `<mode> <endpoint-json> <client-enrollment> [mode-argument]`.
`happy-shutdown` submits work and requests close; `replay-happy-shutdown` also
requires its prior run ID. For these modes, enroll the helper executable instead
of `native_client`. Other modes include negative tests and deliberately retained
processes; they are not a default suite. In particular, `helper-hold` does not exit.

Gap cases require a test-api runtime with launch-only
`--test-mode <mode> <32-lowercase-hex-token>` and an external controller that checks
owner/epoch-bound witnesses and releases only the selected synthetic pauses.
Wire requests cannot enable controls. Do not invent release files to bypass a
timeout or expose test-api binaries as a product runtime.

The relay fault case uses a fresh synthetic fixture and an external controller to
observe active-session retention. Its deliberate nonpassing result and any
verified process disposition are recorded separately from graceful closure.

Migration and FC-13 targets optionally use `YOKAI_TEST_CONTROLLER_DIR` for their
controlled-child request/acknowledgement. Without it, their direct child behavior
remains. Read each target's contract before selection; this is not a general
command dispatcher. Full qualification orchestration remains outside the public
source. This guide documents developer setup, not an equivalent containment suite.

Preserve exact commands/environment, source/features, binary hashes, fixtures,
stdout/stderr, named results and process/Job closure observations. Distinguish
expected refusals from failures. Timeout/termination cannot pass graceful close.
An unobserved controller exit is separate from empty closed case Jobs. Keep private
approvals, machine paths and raw qualification evidence out of public source.
