# Yokai

Yokai is a Windows-first, local-first, model-agnostic application in development.
This checkout provides a Rust storage foundation and a headless conversation
service with a deterministic fake response. On Windows, a separate ordinary-user
runtime exposes that service to an enrolled native client over local named pipes.
A desktop application, running supervisor and real model adapters are future work.

## Implemented foundation

- Instance creation, exclusive ownership locks, path checks, and startup
  compatibility refusals.
- Versioned SQLite migrations with verified pre-upgrade recovery snapshots.
- One owned writer, bounded read workers and queues, explicit deadlines,
  write-outcome classification, and coordinated close.
- Durable command identity and digest checks, atomic command/run/job/event
  records, and conservative startup epochs and holds.
- Typed conversation admission, bounded fake streaming, committed responses
  and replay after reopen. See the [headless demonstration](docs/m1-conversation-core.md).
- Authenticated local connections, bounded messages and explicit runtime
  shutdown/reopen. See the [local runtime contract](docs/m1-local-runtime.md)
  for the trusted same-user boundary, lifecycle behavior and validation limits.

The [store documentation](crates/yokai-store/README.md) describes these contracts
and their limits. Native Windows validation covers selected storage and file
semantics; it does not establish complete Windows qualification, power-loss
durability, or a release-ready application.

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

See [CONTRIBUTING.md](CONTRIBUTING.md) for the validated Windows toolchain,
offline build commands, and review workflow. The
[storage harness guide](crates/yokai-store/tests/README.md) explains test inputs,
platform exclusions, and the extra setup needed for the complete custom suite.

## License

The source is licensed under [Apache License 2.0](LICENSE).
