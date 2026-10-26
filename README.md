# Brick

A Rust bootstrap for independently executable components, starting from standalone
interfaces. Interface changes are explicit coordination points; implementation
behind an accepted interface can proceed independently.

This is a first working increment, not the complete framework. Runtime and provider code in
this repository is Rust; the measurement harness is Python. `protoc` supplies the existing protobuf parser/compiler;
tonic supplies gRPC and prost supplies protobuf encoding and bindings.

## Create a Rust module

```sh
cargo build --locked -p brick-cli
target/debug/brick new module hello --path .local/hello
cd .local/hello
cargo test
```

The scaffold creates a contract crate, an authored implementation crate, and a test
that calls both the real implementation and a generated shadow through one typed
client. `contract/build.rs` calls the `brick-build` library to validate the standalone
contract and generate typed Rust APIs. Ordinary Cargo builds invoke neither protoc
nor services; the descriptor is captured during scaffolding. An outdated descriptor
or wrong provider method signature fails the build.

The first Cargo invocation creates the new workspace's lockfile; commit it and use
`--locked` thereafter. Current scaffolds use explicit local paths to this Brick
checkout (`--brick-root` overrides it); these packages are not published yet.

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
loads the descriptor directly. The `brick-build` library generates typed clients, provider traits, and host adapters
from those descriptors. Brick's compiler boundary and newly scaffolded modules use
these types; the older meta-service entrypoints retain dynamic adapters.

## Generate Brick through Brick

The second real component is **GenerateBundle**. Its [standalone contract](interfaces/generate-bundle.brick.toml)
contains the complete artifact shape and digest rules. The generator returns an
artifact over gRPC; the CLI validates it, checks that it belongs to the submitted
source, and publishes it without overwriting files.

Bootstrap the generator once:

```sh
target/debug/brick generate interfaces/generate-bundle.brick.toml --output .brick/generator.json
target/debug/brick verify .brick/generator.json
target/debug/brick verify .brick/generator.json --mode generate-bundle
target/debug/brick serve .brick/generator.json --mode generate-bundle
```

In another terminal, using the printed port, generate the generator itself:

```sh
target/debug/brick generate interfaces/generate-bundle.brick.toml \
  --via 127.0.0.1:PORT --contract .brick/generator.json \
  --output .brick/generated-generator.json
target/debug/brick verify .brick/generated-generator.json --mode generate-bundle
```

The authored cases cover rejection behavior, so their receipts attest to those
cases only. Success-path tests additionally compare the complete remotely generated
artifact with the seed output, run the returned generator bundle, and generate a
checker bundle that passes real-provider verification. Artifact identities include
the compiler identity, so success comparisons use the same local compiler.

The service response is limited to 1 MiB; larger generated artifacts fail explicitly.
Malformed definitions, schemas, imports, and cases return declared rejection codes;
compiler/IO failures remain RPC failures. The generator publishes no files itself.
Cancelling a remote CLI call leaves the separately owned service running; its active
compiler work remains bounded by the ten-second compiler deadline. Stopping the
service cancels and reaps its compiler. Neither path constitutes an OS sandbox.

## Run verification through Brick

**RunScenarios** is the third real component. Its [standalone contract](interfaces/run-scenarios.brick.toml)
defines local bundle and receipt types without importing either earlier interface.
It owns a private provider per accepted run and returns the exact target bundle,
mode, evidence kind, case count, result, compiler/executable identities, and cleanup.

```sh
target/debug/brick generate interfaces/run-scenarios.brick.toml --output .brick/runner.json
target/debug/brick verify .brick/runner.json --mode run-scenarios
target/debug/brick serve .brick/runner.json --mode run-scenarios
# In another terminal, use the printed port:
target/debug/brick verify .brick/check.json --mode interface-check \
  --via 127.0.0.1:PORT --contract .brick/runner.json
```

The CLI checks receipt consistency and exits unsuccessfully when `passed=false`.
The portable runner examples cover rejection behavior. Integration tests separately
exercise shadow success, real-checker success, deliberately incorrect expectations,
and a generated runner verifying a generated checker. Those tests establish more
than merely replaying the runner's examples.

V0 accepts 1–32 cases, one active run **per runner instance**, and the shadow,
interface-check, or generate-bundle provider. Overlap is rejected as `RUNNER_BUSY`;
recursive runner execution is rejected. Compiler startup has a ten-second subprocess
bound, scenario execution a separate ten-second budget, and cleanup up to four
seconds. This is not a ten-second end-to-end guarantee or a host-wide scheduler.
Remote client cancellation can leave the bounded run executing; stopping the
runner signals its owned run, including compiler discovery. A failed scenario or
execution deadline produces a failed receipt after cleanup; startup/infrastructure
or cleanup failure produces an RPC error without a successful cleanup claim.

## Build Brick with Brick

The first real component is **InterfaceCheck**. Its standalone definition can be
served as a generated shadow, then replaced by its Rust implementation without
changing the consumer contract. That implementation calls the bootstrap checking
kernel; the component tests exercise it over real local gRPC.

The generator now also runs behind its own Brick boundary. Its algorithm still
uses the trusted seed kernel for definition loading, compiler invocation, and
binding generation. The scenario runner also uses the existing seed verification engine through its
own interface. Runtime dispatch and all three algorithms remain seed code. Those are visible bootstrap responsibilities,
not a claim of complete self-hosting. See [the bootstrap plan](docs/bootstrap.md).

## Independent generation and replaceable compilation

The generator now has its own package and executable. Its build graph excludes the
compiler implementation and the bootstrap runtime. It runs its actual generation
algorithm against either a frozen compiler shadow or a real compiler endpoint,
selected through an explicitly pinned standalone contract.

See [the runnable isolation experiment](docs/component-isolation.md) for commands,
acceptance tests, identity rules, and measurement limits.

| Crate | Responsibility |
| --- | --- |
| `brick-contract` | Definition/artifact data, validation, identities, atomic publication |
| `brick-rpc` | Generic descriptor transport and finite shadow host |
| `brick-build` | Build-time contract validation and typed Rust adapter generation |
| `brick-compiler-contract` | Typed compiler boundary, without its implementation |
| `brick-generator` | Generation algorithm and independently runnable provider |
| `brick-compiler` | Independently runnable protoc adapter and process lifecycle |
| `brick-seed` | Explicit bootstrap bridge composing compiler and generator |
| `brick-runtime` | Compatibility harness for built-in providers and scenario runner |
| `brick-cli` | Bootstrap commands and remote generation/verification clients |

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
graph wiring, behavioral-language proofs, stateful
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
