# Yokai

Yokai is a Windows-first, local-first application in development. Try a
conversation with the deterministic fake model in the headless example or
the Windows desktop development client. Both save messages locally and can
restore the committed conversation after reopening.

**Pre-alpha.** Rust APIs, CLI arguments, the local IPC protocol and its JSON
records, and the on-disk Instance layout and SQLite schema are unstable.
Compatibility across revisions is unguaranteed. Use disposable test data.

## Implemented

- Instance creation, exclusive ownership locks, path checks, and startup
  compatibility refusals.
- Versioned SQLite migrations with verified pre-upgrade recovery snapshots.
- One owned writer, bounded read workers and queues, explicit deadlines,
  write-outcome classification, and coordinated close.
- Durable command identity and digest checks, atomic command/run/job/event
  records, and conservative startup epochs and holds.
- A typed conversation service with a deterministic fake response, bounded
  streaming, committed messages and replay after reopen.
- A Windows runtime and sample native client that connect over authenticated
  local named pipes within a trusted Windows user session. The runtime supports
  explicit shutdown and reopen.
- A Windows desktop client for one pre-enrolled local Instance. Close the
  window to leave the runtime running, or quit to request and confirm runtime
  shutdown. [Desktop guide](apps/desktop/README.md)

See the [headless demonstration](docs/m1-conversation-core.md), the
[Windows runtime guide](docs/m1-local-runtime.md) and the
[storage format guide](crates/yokai-store/README.md).

## Tested

Native Windows checks cover selected storage and filesystem behavior,
synthetic conversation completion and replay, and selected local runtime
paths including authenticated connections and ordinary shutdown and reopen.
Desktop checks cover connecting to an enrolled Instance, sending a message
with the fake model, restoring the conversation, and the separate window
close and runtime quit paths.
Power-loss durability and broader Windows storage and lifecycle qualification
remain open. See the [storage harness guide](crates/yokai-store/tests/README.md)
and the [runtime validation boundary](docs/m1-local-runtime.md#validation-boundary).

## Planned

- Automatic process supervision.
- Real model adapters and credential management.

## Workspace layout

| Crate | Current responsibility |
| --- | --- |
| `yokai-store` | Instance lifecycle, migrations, owned SQLite workers, durable records |
| `yokai-platform` | Filesystem locations, Instance locks, randomness, Windows pipe and peer checks |
| `yokai-types` | Shared identifiers and typed conversation requests and events |
| `yokai-testsupport` | Explicit diagnostic harness support |
| `yokai-exec` | Conversation service, deterministic fake and Windows runtime process |
| `yokai-ipc` | Bounded local protocol, handshake and native client example |
| `yokai-supervisor`, `yokai-vault` | Boundary stubs |

Workspace members are explicit in [Cargo.toml](Cargo.toml). The toolchain is
pinned by [rust-toolchain.toml](rust-toolchain.toml), and dependencies by
[Cargo.lock](Cargo.lock).

## Development

See [CONTRIBUTING.md](CONTRIBUTING.md) for native Windows setup, build commands,
and tests that run without historical fixtures. The [desktop build guide](apps/desktop/README.md#build)
covers the separate native and renderer build. See [SECURITY.md](SECURITY.md)
to report a vulnerability privately.

## License

Yokai source is licensed under the [Apache License 2.0](LICENSE).
Third-party dependencies retain their own licenses. See the
[core Rust lockfile](Cargo.lock), [desktop Rust lockfile](apps/desktop/native/Cargo.lock)
and [desktop npm lockfile](apps/desktop/package-lock.json). SQLite is bundled through
`rusqlite` and `libsqlite3-sys`. [SQLite's public-domain dedication](https://www.sqlite.org/copyright.html)
