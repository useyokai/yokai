# Yokai desktop

The Windows desktop opens one pre-enrolled local Instance. Use **Connect**
with a fresh desktop profile.

**Pre-alpha.** The desktop enrollment, restoration records and native
bridge types may change between revisions. Use disposable test data and
build the runtime and desktop from the same source revision.

Press **Ctrl+W** or use the window Close button to close Yokai and leave the
runtime running. Ctrl+W also works while the message box has focus. Reopen
Yokai with the same enrollment while the runtime is running to restore the
saved conversation automatically.

Choose **File > Shut down runtime** to stop the runtime. Yokai checks the
connection and can refresh an expired session on the same runtime. Close the
window after Yokai shows **Runtime stopped**. **Runtime shutdown could not be
confirmed** means the outcome remains uncertain.

Choose **File > Quit Yokai** or press **Ctrl+Q** to shut down the local runtime
and close Yokai. The window closes after shutdown is confirmed. If shutdown
cannot be confirmed, Yokai stays open and shows the status.

## Enrollment

Trusted setup prepares the runtime and the desktop enrollment. See the
[local runtime contract](../../docs/m1-local-runtime.md#enrollment-and-trust) for the
mutual executable pins, Instance identity, and process ownership requirements.

Place `enrollment.json` in the desktop's application-local data directory
(`%LOCALAPPDATA%\com.useyokai.desktop` on Windows). Supply the admitted WebView2
version, the existing runtime control directory, and the desktop's pin for the runtime.

```json
{
  "version": 1,
  "webview_version": "<admitted WebView2 version>",
  "runtime": {
    "control": "<absolute runtime control directory>",
    "enrollment": {
      "peer_image": "<absolute runtime executable path>",
      "peer_sha256": "<lowercase SHA-256 of that executable>",
      "instance": "<canonical Instance ID>"
    }
  }
}
```

Create ordinary `client`, `profile`, and `evidence` directories beneath
`instances/<Instance ID>` in that application-local data directory. Use the
same enrolled Instance and directories for both launches. The desktop guards
the client directory, reads and writes its restoration record there, and creates
a separate diagnostics file for each launch.

## Build

Use the [pinned Windows toolchain and offline prerequisites](../../CONTRIBUTING.md).
The desktop build was checked with Node.js 22.20.0 and npm 10.9.3. It uses
separate native and renderer lockfiles. From this directory, populate the
desktop caches once with network access.

```powershell
cargo fetch --manifest-path native/Cargo.toml --locked --target x86_64-pc-windows-msvc
npm.cmd ci --ignore-scripts --no-audit --no-fund
```

With the caches available, generate the typed frontend contract, build the
packaged renderer, and build the host.

```powershell
New-Item -ItemType Directory -Force src/generated
cargo run --manifest-path native/Cargo.toml -p yokai-desktop-contract --bin export-desktop-contract --features bindings --frozen --locked --offline -- src/generated/desktop-contract.ts
npm.cmd ci --offline --ignore-scripts --no-audit --no-fund
node node_modules/vite/bin/vite.js build
cargo build --manifest-path native/Cargo.toml -p yokai-desktop --bin yokai-desktop --target x86_64-pc-windows-msvc --frozen --locked --offline
```

After building the renderer, run the automated native desktop checks with
a fresh local directory for synthetic restoration records. The tests retain
their fixtures for inspection.

```powershell
$HostFixture = Join-Path ([System.IO.Path]::GetTempPath()) ("yokai-desktop-tests-" + [guid]::NewGuid().ToString('N'))
$env:YOKAI_HOST_TEST_ROOT = (New-Item -ItemType Directory -Path $HostFixture).FullName
cargo test --manifest-path native/Cargo.toml --workspace --target x86_64-pc-windows-msvc --frozen -j 4
```
