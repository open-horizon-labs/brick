# Rust module bootstrap and build enforcement

Brick is a Rust/Cargo framework for independently buildable and verifiable components.
Cargo owns package resolution/builds, Git owns source versions, and Brick provides
boundary validation, typed adapters, runnable shadows and explicit verification.
It does not schedule or run agents.

## The build library

A contract crate uses the following build hook:

```rust
fn main() {
    brick_build::contract("interface.brick.toml", "interface.bundle.json")
        .expect("standalone module contract");
}
```

Its library includes the generated module:

```rust
include!(env!("BRICK_CONTRACT_RS"));
```

`brick-build` checks the frozen artifact's integrity, enforces import-free schema
shape, requires the authored schema to match the frozen descriptor's recorded
schema, and validates declared cases against that descriptor. It uses the existing
tonic/prost generator for Rust messages, clients and provider traits, and adds a
small adapter for Brick's bounded host. Output is content-addressed under OUT_DIR;
it does not overwrite authored files or invoke protoc, Cargo, services or tests.

This is compile-time contract enforcement, not a sandbox or proof that arbitrary
Cargo dependency declarations respect every architectural rule. Provider signatures
are checked by Rust. Complete consumer/provider build separation must still be
preserved in Cargo dependencies. The compiler contract's normal/build graph contains
no compiler or generator implementation.

## The generated surface

- DTOs and tonic provider trait: compile-time request/response types.
- `Adapter::new(provider)`: typed provider behind the existing generic RPC host.
- `Client::connect(address)`: typed client with loopback restriction, contract
  revision metadata, request timeout and message-size limits.

The adapter passes the host's cancellation token in request extensions. Providers
that spawn owned work, such as the compiler adapter, use that token. Generic host
admission remains owned by the work through completion even if the client disconnects.
Raw tonic-generated clients/server types are also available; the Brick client/adapter
is the supported path for preserving these runtime conventions.

Current typed codegen supports one standalone unary service, with top-level
request/response messages. `Client` and `Adapter` are reserved helper names. This
is a deliberately bounded generator, not a compiler for arbitrary Rust traits.
Existing tonic macros provide async trait implementation; no custom Brick proc
macro was needed for this increment.

## Scaffold

```sh
brick new module payments --path ./payments
cd payments
cargo test
cargo run -p payments-implementation
```

Creation requires protoc once to capture the initial descriptor. It validates the
name and refuses an existing destination. The workspace has separate `contract/`
and `implementation/` crates, a starter echo behavior, pinned direct dependencies,
a two-job build setting, and a typed real/shadow conformance test. Authored provider
code is never regenerated. A `.brick-incomplete` marker remains if filesystem
publication fails; that directory must not be treated as a completed scaffold.

The first Cargo command resolves Cargo.lock. Commit it and use --locked afterward.
The generated manifest explicitly references the local Brick source checkout;
`--brick-root` selects another checkout. These are bootstrap paths, not portable
published dependency claims.

To evolve an interface, generate a new bundle at a new path, review it, then point
the build hook and harness at it. Keeping the old descriptor with a changed schema
fails immediately. Scenario-only edits produce the same typed Rust code identity;
scenario execution still uses the explicitly selected bundle. Regenerate/reselect
that bundle when changing runtime scenarios.

## Acceptance

`tools/check-scaffold.py` creates a separate ignored workspace, verifies its real
provider and shadow through the same typed client, and blocks protoc throughout
Cargo builds. It changes the provider's response type and expects compilation to
fail, then changes the schema while retaining the old bundle and expects build.rs
to fail. It restores the files and reruns the suite. Outputs remain under
`.local/scaffold-*/`. Existing generator isolation tests now exercise the typed
compiler client against the frozen compiler shadow.
