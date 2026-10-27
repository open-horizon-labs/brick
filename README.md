# Brick

Brick is an experimental Rust framework built around one principle:

**You should be able to build and test a component without building or running
its dependencies’ implementations.**

Start with a standalone interface and examples of its behavior. Brick turns those
into a runnable **shadow**: a shell that answers the declared requests and fails
on anything unexpected. A component can use these shadows while the real
dependencies are still being written.

Once the interfaces are agreed, agents can work behind them independently.
Changing an interface is an explicit coordination point.

## Why

A hundred agents sharing a machine still share its memory and disks. If every
small change rebuilds half the application or starts the whole system to test it,
more agents just mean more waiting.

The project that prompted Brick targets five-minute verification, including
queueing and cleanup. Each project sets its own budget. We haven’t demonstrated
that target with a hundred agents.

And this is useful with one developer, too. A dependency’s implementation should
be replaceable without becoming part of your edit-build-test loop.

## The rules

- **Interfaces stand alone.** Each definition contains its entire schema and
  scenarios. No schema imports or shared domain types, even when two interfaces
  describe the same thing. Generated code can use ordinary runtime libraries.
- **Shadows run before implementations exist.** They use explicit, finite scenarios.
  Unspecified behavior fails. A passing shadow test checks the declared interaction;
  the real dependency still needs integration tests.
- **Implementations stay separate.** Consumers depend on the contract. Cargo
  dependencies must preserve that separation so changing a provider doesn’t force
  its consumers to rebuild.
- **Use Cargo and Git.** Cargo owns builds and dependency versions. Git identifies
  the source. Brick adds contract checks and executable shadows.

## Try it

Install Rust 1.88 or newer and `protoc`, then:

```sh
git clone https://github.com/open-horizon-labs/brick.git
cd brick
cargo build --locked -p brick-cli
target/debug/brick new module hello --path .local/hello
cd .local/hello
cargo test
```

This creates separate contract and implementation crates. The test calls the real
implementation and a generated shadow through the same typed client.

The contract crate’s `build.rs` generates the Rust API from a saved descriptor.
A changed schema with an old descriptor fails the build; Rust rejects a provider
with the wrong method signature. Ordinary builds need no `protoc` or running
services. Scaffolding uses `protoc` once.

Scaffolds currently reference your local Brick checkout. Keep it available, commit
the new project’s `Cargo.lock` after the first build, and use `--locked` afterward.
The crates aren’t published yet.

## Where it stands

Brick’s own generator builds independently of its compiler implementation and can
run against a compiler shadow. Other parts still use bootstrap code. The
[isolation experiment](docs/component-isolation.md) records what we measured and
what remains unproven.

The prototype supports standalone protobuf interfaces with unary RPCs. Shadows
replay ordered scenarios once each. Build checks enforce contracts; they don’t
police every Cargo dependency. Resource scheduling and shared-cache management
remain open work.

- [Rust modules and build checks](docs/rust-modules.md)
- [CLI commands and runtime limits](docs/cli-guide.md)
- [Building Brick with Brick](docs/bootstrap.md)

To check the repository, run these from its root:

```sh
cargo fmt --all --check
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets -- -D warnings
```

## License

[MIT](LICENSE).
