use anyhow::{Context, Result, ensure};
use std::{collections::BTreeMap, fs, io::Write, path::Path};

pub fn module(name: &str, path: &Path, brick_root: &Path) -> Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 48
            && name.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            && !name.ends_with('-')
            && !name.contains("--"),
        "module name must be lowercase kebab-case, starting with a letter, up to 48 characters"
    );
    ensure!(
        !path.exists(),
        "refusing existing destination {}",
        path.display()
    );
    let root = brick_root.canonicalize().context("Brick source root")?;
    for crate_name in ["brick-build", "brick-rpc", "brick-contract"] {
        ensure!(
            root.join("crates")
                .join(crate_name)
                .join("Cargo.toml")
                .is_file(),
            "missing {crate_name} under Brick source root"
        );
    }
    let schema = format!(
        r#"syntax = "proto3";
package brick.modules.{}.v1;
service Module {{ rpc Apply(Request) returns (Reply); }}
message Request {{ string text = 1; }}
message Reply {{ string text = 1; }}
"#,
        name.replace('-', "_")
    );
    let definition = format!(
        "format_version = 1\nschema = '''\n{schema}'''\n\n[[cases]]\nname = \"starter echo\"\nmethod = \"Apply\"\nrequest_json = '{{\"text\":\"hello\"}}'\nresponse_json = '{{\"text\":\"hello\"}}'\n"
    );
    // Bootstrap once here. Subsequent Cargo builds consume this frozen descriptor.
    let bundle = brick_seed::build(&definition)?;
    let local = |crate_name: &str| {
        serde_json::to_string(&root.join("crates").join(crate_name).to_string_lossy())
            .expect("path JSON")
    };
    let mut files = BTreeMap::<String, String>::new();
    files.insert("Cargo.toml".into(),format!(r#"[workspace]
members = ["contract", "implementation"]
resolver = "3"
[workspace.package]
version = "0.1.0"
edition = "2024"
rust-version = "1.88"
publish = false
[workspace.dependencies]
brick-build = {{ path = {} }}
brick-rpc = {{ path = {} }}
brick-contract = {{ path = {} }}
anyhow = "=1.0.104"
prost = "=0.14.3"
prost-reflect = {{ version = "=0.16.5", features = ["serde"] }}
serde_json = "=1.0.150"
tonic = "=0.14.5"
tonic-prost = "=0.14.5"
tokio = {{ version = "=1.52.3", features = ["macros", "rt-multi-thread", "net", "signal", "sync", "time"] }}
[workspace.lints.rust]
unsafe_code = "forbid"
[profile.dev]
debug = 0
incremental = false
[profile.test]
debug = 0
incremental = false
"#,local("brick-build"),local("brick-rpc"),local("brick-contract")));
    files.insert(".cargo/config.toml".into(), "[build]\njobs = 2\n".into());
    files.insert(".gitignore".into(), "/target/\n/.local/\n".into());
    files.insert(
        "contract/Cargo.toml".into(),
        format!(
            r#"[package]
name = "{name}-contract"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
publish.workspace = true
[dependencies]
brick-rpc.workspace = true
anyhow.workspace = true
prost.workspace = true
prost-reflect.workspace = true
tonic.workspace = true
tonic-prost.workspace = true
[build-dependencies]
brick-build.workspace = true
[lints]
workspace = true
"#
        ),
    );
    files.insert("contract/build.rs".into(),"fn main() {\n    brick_build::contract(\"interface.brick.toml\", \"interface.bundle.json\").expect(\"standalone module contract\");\n}\n".into());
    files.insert("contract/src/lib.rs".into(),"//! Generated typed boundary. Implementation dependencies do not belong here.\ninclude!(env!(\"BRICK_CONTRACT_RS\"));\n".into());
    files.insert("contract/interface.brick.toml".into(), definition);
    files.insert(
        "contract/interface.bundle.json".into(),
        serde_json::to_string_pretty(&bundle)?,
    );
    files.insert(
        "implementation/Cargo.toml".into(),
        format!(
            r#"[package]
name = "{name}-implementation"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
publish.workspace = true
[dependencies]
contract = {{ package = "{name}-contract", path = "../contract" }}
brick-contract.workspace = true
brick-rpc.workspace = true
anyhow.workspace = true
serde_json.workspace = true
tonic.workspace = true
tokio.workspace = true
[lints]
workspace = true
"#
        ),
    );
    files.insert(
        "implementation/src/lib.rs".into(),
        r#"//! Authored implementation. Brick never regenerates this file.
pub struct Module;
#[tonic::async_trait]
impl contract::module_server::Module for Module {
    async fn apply(&self, request: tonic::Request<contract::Request>)
        -> Result<tonic::Response<contract::Reply>, tonic::Status> {
        Ok(tonic::Response::new(contract::Reply { text: request.into_inner().text }))
    }
}
"#
        .into(),
    );
    let implementation = name.replace('-', "_") + "_implementation";
    files.insert("implementation/src/main.rs".into(),format!(r#"use anyhow::Result;
#[tokio::main(worker_threads = 2)]
async fn main() -> Result<()> {{
    let bundle: brick_contract::Bundle = serde_json::from_str(include_str!("../../contract/interface.bundle.json"))?;
    let mut server = brick_rpc::start(&bundle,
        std::sync::Arc::new(contract::Adapter::new({implementation}::Module)),
        "127.0.0.1:0".parse()?).await?;
    println!("{{}}", server.address);
    tokio::signal::ctrl_c().await?;
    server.shutdown().await
}}
"#));
    files.insert("implementation/tests/conformance.rs".into(),format!(r#"use std::sync::Arc;
#[tokio::test]
async fn typed_client_checks_real_provider_and_generated_shadow() {{
    let bundle: brick_contract::Bundle = serde_json::from_str(include_str!("../../contract/interface.bundle.json")).unwrap();
    for real in [false, true] {{
        let provider: Arc<dyn brick_rpc::Handler> = if real {{
            Arc::new(contract::Adapter::new({implementation}::Module))
        }} else {{
            Arc::new(brick_rpc::Shadow::new(&bundle).unwrap())
        }};
        let mut server = brick_rpc::start(&bundle, provider, "127.0.0.1:0".parse().unwrap()).await.unwrap();
        let mut client = contract::Client::connect(server.address).await.unwrap();
        let result = client.apply(contract::Request {{ text: "hello".into() }}).await;
        // Clean up before inspecting assertions, including a failing implementation.
        let complete = server.assert_complete();
        server.shutdown().await.unwrap();
        assert_eq!(result.unwrap().text, "hello");
        complete.unwrap();
    }}
}}
"#));
    files.insert(
        "README.md".into(),
        format!(
            r#"# {name}

A standalone Rust component workspace created by `brick new module`.

```sh
cargo test
cargo run -p {name}-implementation
```

The first Cargo command creates Cargo.lock; commit it and use `--locked` thereafter.
`contract/build.rs` invokes brick-build to enforce a standalone contract and generate
typed clients/provider adapters. Cargo builds need no protoc or running services.
The implementation is authored separately; consumers depend only on `{name}-contract`.
The conformance test calls both the real provider and a generated shadow through
the same typed client. Its passing shadow case proves wiring; the real case checks
the starter echo behavior. Author additional expectations when adding behavior.

To change the contract, edit contract/interface.brick.toml, then generate a NEW bundle:

```sh
brick generate contract/interface.brick.toml --output contract/next.bundle.json
```

Review that artifact and explicitly update build.rs and the harness paths to use it.
Generation never overwrites an existing artifact or authored implementation.
A changed schema with the old bundle fails at build time. Code is generated into
content-addressed files under Cargo OUT_DIR. The current typed surface accepts
standalone unary services with top-level request/response messages.

Bootstrap limitation: Cargo.toml uses explicit local paths to the Brick source
checkout at `{}`. These are not published packages. Move/pin these dependencies
explicitly before sharing this workspace. There is no production deployment,
network sandbox, or agent scheduler supplied by this scaffold.
"#,
            root.display()
        ),
    );
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    fs::create_dir(path)
        .with_context(|| format!("refusing existing destination {}", path.display()))?;
    let marker = path.join(".brick-incomplete");
    fs::OpenOptions::new().write(true).create_new(true).open(&marker)?.write_all(b"Scaffold publication in progress. A remaining marker means creation did not complete.\n")?;
    for (relative, contents) in files {
        let target = path.join(relative);
        fs::create_dir_all(target.parent().context("parent")?)?;
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)?
            .write_all(contents.as_bytes())?;
    }
    fs::remove_file(marker)?;
    Ok(())
}
