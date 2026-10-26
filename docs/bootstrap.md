# Bootstrapping Brick in Rust

The objective is to build Brick using its own interface-first workflow. A small
trusted seed must exist before Brick can execute itself; hide neither that seed
nor its remaining responsibilities.

## First increment

1. Author the standalone `InterfaceCheck` contract and examples.
2. Seed compiles it with `protoc`, validates descriptor isolation, generates Rust
   message bindings, and publishes one immutable descriptor bundle.
3. Descriptor runtime starts its shadow without compiling a provider.
4. Real Rust provider replaces that shadow using the same contract.
5. Runner verifies expected behavior over gRPC and records exact evidence.

This establishes one real component behind a Brick boundary. It is partial
self-hosting, not yet a self-building compiler or a self-hosted orchestration graph.

Alternatives considered: wrapping an external mock engine (faster scaffolding but
does not exercise a Rust-owned shadow runtime); generating and compiling a Rust
server per shadow (unnecessary repeated compilation); dynamic descriptor serving
using established protobuf and gRPC libraries (selected). The seed never implements
its own protobuf parser or gRPC framing.

## Second increment: implemented

`GenerateBundle` now has a standalone definition, generated shadow, and real Rust
adapter. `generate --via ADDRESS --contract BUNDLE` uses that boundary and publishes
only after validating the returned artifact and matching its source to the request.
The service never chooses publication paths. Tests compare its full successful
output with the seed, run the returned generator bundle, and use the generator to
produce a runnable checker. Rejection cases cover invalid definitions, imports,
and schemas; additional tests cover invalid cases, substitution, tampering,
nonclobber publication, and cancellation during generator compiler execution.

This moves generation behind an executable interface; it does not remove the
trusted generation algorithm from the seed or create a fully self-hosted build.
The provider is selected explicitly in the bootstrap runtime. Shadows remain
finite ordered examples. Successful generated artifacts depend on the actual
compiler identity, so success equivalence is tested separately from the portable
static rejection examples. The 1 MiB RPC ceiling is narrower than the 8 MiB disk
bundle ceiling and is an explicit bootstrap transport limit.

## Next migration sequence
- RunScenarios is now independently defined and executable as shadow or real
  provider. Its real adapter owns isolated verification and returns checked receipts;
  the implementation still delegates to the seed verification engine.
- The generator and compiler now have independent packages and executable providers,
  with an explicit pinned dependency. See [component isolation](component-isolation.md).
  The old mode dispatch remains a bootstrap compatibility path. Extend this pattern
  only when another component needs the same independence.
- Add explicit project graph wiring, provider selection, typed generated clients
  and provider traits, and stateful scenario support as independent increments.
- Use those components to generate and verify Brick itself. Reduce the seed to
  loading versioned artifacts, launching providers, and bootstrapping the graph.
- Add resource admission, cache ownership, and host isolation as explicit policies
  and components. Measure resource behavior rather than assuming interface
  separation alone guarantees concurrent execution.

An interface import is never an acceptable shortcut. Identically shaped types
in different interfaces remain independent. Adapters and compatibility checks
connect them explicitly. Each change to a boundary is proposed, regenerated, and
verified against affected consumers before acceptance.

## Acceptance and adversarial checks

| Risk | Required evidence |
| --- | --- |
| Standalone is only a convention | Reject ordinary and well-known-type imports, even with comments/whitespace |
| Shadows require provider compilation | Serve generated bundles without creating/building a new crate |
| Generated success is mistaken for correctness | Wrong expected response passes shadow wiring but fails real checker verification |
| A shadow silently accepts extra calls | Wrong, exhausted, and unconsumed expectations fail |
| Regeneration overwrites authored work | Existing output is preserved and publication fails |
| Evidence is stale or modified | Bundle tampering fails digest validation; scenario changes change identity |
| Runtime leftovers recreate contention | Success/failure/cancellation close the owned listener; separate runs do not share scenario state |

The initial finite examples are a deliberately limited scenario language. Database
transactions, provider outcomes, effect permits, and complex concurrent state
machines require later contract-specific adapters and tests, including Qlarix's
delivery pilot. The initial seed cannot claim those capabilities.
