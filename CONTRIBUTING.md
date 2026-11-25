# Contributing

Keep changes small and reviewable, and describe the behavior and validation
scope a reviewer can establish from the change.

## Windows build prerequisites

Install [Rustup](https://rustup.rs/) and Visual Studio or Visual Studio Build
Tools with the **Desktop development with C++** workload and a Windows SDK.
Open **Developer PowerShell for VS** with the x64 host and target. Run the
commands from the repository root. The recorded native build used Rust and
Cargo **1.99.0**, MSVC **14.51.36231**, and the **10.0.26100.0** SDK.

For a fresh checkout with network access, install the pinned toolchain and
download the locked dependencies once. [Cargo fetch documentation](https://doc.rust-lang.org/cargo/commands/cargo-fetch.html)

```powershell
rustup toolchain install 1.99.0-x86_64-pc-windows-msvc --profile minimal
rustc -Vv
cargo fetch --locked --target x86_64-pc-windows-msvc
```

Check that `rustc -Vv` reports release `1.99.0` and host
`x86_64-pc-windows-msvc`. The `--frozen` flag uses the checked-in lockfile and
the local cache without network access.

## Build and run focused tests

```powershell
cargo check --workspace --all-targets --target x86_64-pc-windows-msvc --frozen -j 4
cargo check --workspace --all-targets --features yokai-store/test-api,yokai-exec/test-api --target x86_64-pc-windows-msvc --frozen -j 4
cargo build --workspace --release --target x86_64-pc-windows-msvc --frozen -j 4
```

The second command also checks compilation of the targets gated by
`test-api`.

Run these two checks with synthetic inputs. The conversation harness prints
three `CHECK ... PASS` lines. The protocol test reports seven passing tests.
Both checks run in memory. Use the Windows opt-in in a dedicated shell for
synthetic tests because the harness cannot disable process dumps on Windows.

```powershell
$env:YOKAI_TEST_ALLOW_UNPROTECTED_WINDOWS = '1'
cargo test -p yokai-types --test conversation_types --target x86_64-pc-windows-msvc --frozen -j 4
cargo test -p yokai-ipc --features test-api --test protocol --target x86_64-pc-windows-msvc --frozen -j 4
```

See the [conversation core guide](docs/m1-conversation-core.md) for the headless
example and the [focused harnesses](crates/yokai-store/tests/README.md#focused-conversation-checks)
for checks that use fresh synthetic data.

## Local runtime builds and checks

The Windows targets include `yokai-runtime`, the `native_client` example,
protocol/adapter tests and a custom cross-process helper. `yokai-exec/test-api`
also enables the store, IPC and platform test APIs. Do not ship a runtime with
these fixture controls enabled.

Keep compilation separate from bounded runtime validation. Compile tests with
`--no-run`, record the exact executable from Cargo JSON output, and invoke that
executable directly during the runtime phase. `cargo test` without `--no-run`
and `cargo run` can compile and do not belong in that phase. Compilation does
not authorize every generated target.

The [local runtime testing guide](docs/m1-local-runtime-testing.md) describes
profiles, enrollment, fixture setup and evidence limits. `local_runtime_windows`
requires an explicit case mode and externally owned runtime; it is not a
self-contained suite. Do not use an unrestricted workspace test command to
reproduce the selected matrix. Storage checks below have their own side effects.

## Running storage checks

The custom harnesses require the explicit `test-api` feature and an absolute,
existing `YOKAI_TEST_DATA_DIR` dedicated to synthetic data. Use a fresh directory
outside the checkout on fixed NTFS for the recorded Windows profile. The tests
retain fixtures and fault evidence; plan for their storage use.

Windows dumpability protection is unsupported. Set
`YOKAI_TEST_ALLOW_UNPROTECTED_WINDOWS=1` only for synthetic test data. This
opt-in does not disable Windows Error Reporting or prevent crash dumps.

After setting `YOKAI_TEST_DATA_DIR`, this command selects the self-contained
SQLite target. It checks the bundled engine's version and source ID along
with file-backed behavior. The dangling-link case records whether Windows
allowed symlink creation.

```powershell
if (-not $env:YOKAI_TEST_DATA_DIR) { throw 'Set YOKAI_TEST_DATA_DIR to the dedicated test directory.' }
$env:YOKAI_TEST_ALLOW_UNPROTECTED_WINDOWS = '1'
cargo test -p yokai-store --features test-api --test sqlite_runtime --target x86_64-pc-windows-msvc --frozen -j 4
if ($LASTEXITCODE -ne 0) { throw 'SQLite validation failed; preserve the output and test directory.' }
```

The full six-target suite additionally needs pinned historical fixtures and
executables, and a reviewed Windows ACL cleanup procedure. Its complete setup
is described in the [harness guide](crates/yokai-store/tests/README.md).
A successful developer command alone does not reproduce the recorded batch's
input pinning, output gate, process limits, cleanup, and evidence custody.

## Change and review expectations

- Keep production interfaces typed. Raw SQL access, fault injection, and
  observation hooks remain behind `test-api`, which is not a production feature.
- Add a new numbered migration when the schema changes. Preserve existing
  migration bytes and history checksums; document upgrade and refusal behavior.
- Test the relevant failure and recovery behavior with owned synthetic fixtures.
  Record the actual execution platform and any subchecks reported as
  `NOT_CHECKED`; WSL parsing of Windows logs is not a Linux runtime test.
- State remaining uncertainty accurately, especially around ambiguous writes,
  process termination, filesystem effects, and durability.
- Include only maintainable source and developer documentation. Keep raw logs,
  caches, binaries, local paths, private notes, credentials, and real Instance
  data out of commits and issue attachments.
- Preserve third-party license and attribution notices. In the pull request,
  identify upstream sources for copied code and explain dependency changes.
- Follow [SECURITY.md](SECURITY.md) for suspected vulnerabilities.

Submit a pull request with the concrete change, relevant checks, and material
limits. Documentation changes that leave executable inputs unchanged need
documentation checks rather than another runtime qualification batch.

## Merging pull requests

- **Squash and merge** self-contained PRs when intermediate commits are fixups,
  review follow-ups, or work in progress. Use a commit message that describes
  the change as merged.
- **Create a merge commit** when the original commits serve as validation
  checkpoints or when follow-up work depends on the branch’s ancestry. Identify
  the relevant commits and checks in the PR.
- **Record the revision actually tested.** Tie validation results to the exact
  commit or source snapshot used. If a result is reported for the landed
  revision, verify that revision and record the checks run.
