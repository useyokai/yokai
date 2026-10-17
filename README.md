# Yokai

Yokai is a Windows-first, local-first, model-agnostic application in development.
This checkout currently provides a Rust storage foundation. It does not yet
contain a desktop application, a running supervisor, model adapters, or a
conversation interface.

## Implemented foundation

- Instance creation, exclusive ownership locks, path checks, and startup
  compatibility refusals.
- Versioned SQLite migrations with verified pre-upgrade recovery snapshots.
- One owned writer, bounded read workers and queues, explicit deadlines,
  write-outcome classification, and coordinated close.
- Durable command identity and digest checks, atomic command/run/job/event
  records, and conservative startup epochs and holds. Jobs are records;
  admission, claiming, execution, and dispatch are not implemented APIs.

The [store documentation](crates/yokai-store/README.md) describes these contracts
and their limits. Native Windows validation covers selected storage and file
semantics; it does not establish complete Windows qualification, power-loss
durability, or a release-ready application.

## Workspace layout

| Crate | Current responsibility |
| --- | --- |
| `yokai-store` | Instance lifecycle, migrations, owned SQLite workers, durable records |
| `yokai-platform` | Filesystem locations, Instance locks, and randomness |
| `yokai-types` | Shared Instance and command identifiers |
| `yokai-testsupport` | Explicit diagnostic harness support |
| `yokai-exec`, `yokai-ipc`, `yokai-supervisor`, `yokai-vault` | Boundary stubs; runtime services are not implemented |

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
