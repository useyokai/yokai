# Yokai

Yokai is a Windows-first, local-first application in development. The current
checkout lets developers try a headless conversation, save it locally, and
replay the committed messages after reopening.

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

See the [headless demonstration](docs/m1-conversation-core.md), the
[Windows runtime guide](docs/m1-local-runtime.md) and the
[storage format guide](crates/yokai-store/README.md).

## Tested

Native Windows checks cover selected storage and filesystem behavior,
synthetic conversation completion and replay, and selected local runtime
paths including authenticated connections and ordinary shutdown and reopen.
Power-loss durability and broader Windows storage and lifecycle qualification
remain open. See the [storage harness guide](crates/yokai-store/tests/README.md)
and the [runtime validation boundary](docs/m1-local-runtime.md#validation-boundary).

## Planned

- Desktop application and process supervision.
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
and tests that run without historical fixtures. See [SECURITY.md](SECURITY.md)
to report a vulnerability privately.

## License

Yokai source is licensed under the [Apache License 2.0](LICENSE).
Third-party dependencies retain their own licenses. The Rust dependencies are
pinned in [Cargo.lock](Cargo.lock). SQLite is bundled through
`rusqlite` and `libsqlite3-sys`. [SQLite's public-domain dedication](https://www.sqlite.org/copyright.html)
