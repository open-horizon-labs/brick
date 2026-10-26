# Brick development

- Brick is written in Rust and aims to build itself using standalone interface
  definitions and replaceable generated shadows. Be explicit about trusted seed
  responsibilities and incomplete self-hosting.
- Every authored interface is self-contained: no schema imports, shared contract
  types, well-known-type exceptions, or externally supplied semantic obligations.
- Keep business/real-provider code out of generated files. Generation must not
  overwrite existing files. Definitions and complete generation inputs identify
  artifacts and verification evidence.
- Distinguish shadow wiring from real implementation verification. Never turn an
  unspecified shadow response into invented success.
- Reuse protobuf/RPC libraries. Avoid custom wire protocols and unnecessary new
  infrastructure. Separate component code from the generic runtime.
- Respect ongoing work and resource limits. Use the repository's two-job build
  configuration. Do not spawn service processes or compilers without bounded
  lifecycle/cleanup. Do not touch other projects' processes or build caches.
- Run `cargo fmt --all --check`, meaningful targeted tests, and
  `cargo clippy --workspace --all-targets -- -D warnings` for implementation changes.
- Report what is measured, implemented, proposed, or unverified separately.
