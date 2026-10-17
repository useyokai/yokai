# Contributing

The current checkpoint is the storage foundation through PR #23, recorded on
2026-10-17. Keep changes small and reviewable, and describe the behavior and
validation scope a reviewer can establish from the change.

## Windows build prerequisites

The recorded native profile uses Rust and Cargo **1.99.0**, target
`x86_64-pc-windows-msvc`, and an x64 MSVC developer environment with the Windows
SDK. The recorded compiler tools were MSVC 14.51.36231. The linked bundled
SQLite engine reported **3.53.2**; the runtime harness checks its version,
source ID, and required compile options. A system SQLite installation is not
used by the configured `rusqlite` dependency.

Have the pinned Rust toolchain, MSVC tools, and locked dependency cache available
before running offline commands. Cargo's offline flags do not provision those
prerequisites or constrain OS filesystem/network access. A fresh cache with
missing archives will fail; do not silently switch to an online build.

From the repository root in the configured native developer environment:

```powershell
cargo check --workspace --all-targets --offline --locked --frozen -j 4
cargo check --workspace --all-targets --features yokai-store/test-api --offline --locked --frozen -j 4
cargo build --workspace --release --offline --locked --frozen -j 4
```

The default checks omit custom targets that require `test-api`. The second
command compiles those targets without executing them. These command profiles
passed in the recorded Windows checkpoint; this documentation change does not
run them again.

## Running storage checks

The custom harnesses require the explicit `test-api` feature and an absolute,
existing `YOKAI_TEST_DATA_DIR` dedicated to synthetic data. Use a fresh directory
outside the checkout on fixed NTFS for the recorded Windows profile. The tests
retain fixtures and fault evidence; plan for their storage use.

Windows dumpability protection is unsupported. Set
`YOKAI_TEST_ALLOW_UNPROTECTED_WINDOWS=1` only for synthetic test data. This
opt-in does not disable Windows Error Reporting or prevent crash dumps.

After setting `YOKAI_TEST_DATA_DIR`, this command selects the self-contained
SQLite target:

```powershell
if (-not $env:YOKAI_TEST_DATA_DIR) { throw 'Set YOKAI_TEST_DATA_DIR to the dedicated test directory.' }
$env:YOKAI_TEST_ALLOW_UNPROTECTED_WINDOWS = '1'
cargo test -p yokai-store --features test-api --test sqlite_runtime --offline --locked --frozen -j 4
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

Submit a pull request with the concrete change, relevant checks, and material
limits. Documentation changes that leave executable inputs unchanged need
documentation checks rather than another runtime qualification batch.
