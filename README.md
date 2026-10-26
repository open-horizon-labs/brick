# Brick

A Rust bootstrap for independently executable components, starting from standalone
interfaces. Interface changes are explicit coordination points; implementation
behind an accepted interface can proceed independently.

This is a first working increment, not the complete framework. All Brick code in
this repository is Rust. `protoc` supplies the existing protobuf parser/compiler;
tonic supplies gRPC and prost supplies protobuf encoding and bindings.

## Start

Requires Rust 1.88 or newer and `protoc` on PATH. Dependencies are pinned and the
Cargo lockfile is part of the project. Builds use two jobs and omit debug info to
keep the bootstrap's resource demand bounded.

```sh
cargo build --locked -p brick-cli
target/debug/brick check interfaces/interface-check.brick.toml
target/debug/brick generate interfaces/interface-check.brick.toml --output .brick/check.json
target/debug/brick verify .brick/check.json
target/debug/brick verify .brick/check.json --mode interface-check
```

Generation refuses to overwrite any existing output, including handwritten
files. Omit `--output` to use a content-addressed `.brick/<digest>.json` path.
Reuse an existing bundle explicitly when repeating the commands.

The first verification runs a generated shadow. The second runs Brick's real
Rust interface checker through exactly the same descriptor-defined gRPC contract.
The declared cases distinguish a valid standalone schema, an illegal import, and
an invalid schema. An intentionally incorrect expected result can pass shadow
wiring verification but fails against the real implementation: those are different
forms of evidence.

For an interactive endpoint:

```sh
target/debug/brick serve .brick/check.json --mode interface-check
# Use the loopback address printed by serve:
target/debug/brick call .brick/check.json 127.0.0.1:PORT Check '{"schema":"invalid protobuf"}'
```

Ctrl-C stops the service. `verify` uses an ephemeral private listener and shuts it
down after success or failure. No Docker, database, credentials, or generated
service-specific compilation is needed to serve a shadow.

## A definition is standalone

Each `.brick.toml` file contains its entire protobuf schema and declared examples.
It imports nothing. All interface message types are local, including identifiers,
timestamps, and errors. There are no well-known-type or shared-schema exceptions.
Generated code may use pinned runtime libraries; those are not contract imports.

See [the minimal echo definition](interfaces/echo.brick.toml) and
[Brick's own checker definition](interfaces/interface-check.brick.toml).

The seed currently accepts proto3, exactly one service per file, and unary methods.
An example specifies a method, request JSON, and expected response JSON. The server
consumes examples in their declared order, once each. Unexpected requests and
exhausted scenarios fail. A method with no declared behavior returns
`SHADOW_UNSPECIFIED`, never synthetic success. Unconsumed expectations make
verification fail.

The bundle contains the original definition, descriptor, generated Rust **message**
bindings, compiler/generator identities, and a digest over those contents. Serving
loads the descriptor directly. Typed provider/client code generation is not yet
implemented; the dynamic client and server already use the same wire contract.

## Build Brick with Brick

The first real component is **InterfaceCheck**. Its standalone definition can be
served as a generated shadow, then replaced by its Rust implementation without
changing the consumer contract. That implementation calls the bootstrap checking
kernel; the component tests exercise it over real local gRPC.

The trusted seed still owns definition loading, compiler invocation, generation,
runtime dispatch, and the test runner. Those are visible bootstrap responsibilities,
not a claim of complete self-hosting. See [the bootstrap plan](docs/bootstrap.md).

The crates have separate roles:

| Crate | Responsibility |
| --- | --- |
| `brick-contract` | Standalone definition validation, descriptors, generated bindings, atomic bundle publication |
| `brick-runtime` | Descriptor-driven shadow serving, first real provider, gRPC calls, verification receipts |
| `brick-cli` | Command parsing and lifecycle orchestration |

## Evidence and limits

Receipts distinguish `shadow_wiring_verified` from `component_contract_verified`,
and include the bundle/executable digests, actual runtime compiler identity when
used, mode, case count, duration, result, and cleanup status. The real checker pins
its compiler for the run; compiler execution failures fail verification rather
than pretending that the submitted schema is invalid. Every compiler invocation,
including version lookup, has a ten-second deadline. Cancellation signals active
compiler work and reaps its process before normal CLI teardown.
A bundle digest is an integrity/content identity, not a signature
or proof that its author is trustworthy.

This increment does **not** implement arbitrary real-provider loading, multi-component
graph wiring, generated typed service traits, behavioral-language proofs, stateful
failure scripts, virtual time, an OS sandbox, scheduler fairness, or a distributed
cache. Loopback binding and a clean compiler environment do not establish a network
sandbox. It has no production deployment surface.

The five-minute feedback target belongs to a project policy. This seed has no
credible 100-agent throughput measurement. Its own test suite and build timings
must not be presented as such a measurement.

## Check the increment

```sh
cargo fmt --all --check
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets -- -D warnings
```

Tests must reject architectural violations and incorrect expectations, not merely
confirm that the generated shell echoes the examples it was given.
