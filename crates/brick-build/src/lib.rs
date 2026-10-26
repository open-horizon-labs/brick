//! Cargo build hook: validates a standalone contract and emits typed Rust adapters.
//! Consumes a frozen descriptor bundle; never discovers protoc or starts a service.
use anyhow::{Context, Result, ensure};
use brick_contract::{Bundle, contract_id, hash, parse, schema_input, validate_cases};
use heck::{ToSnakeCase, ToUpperCamelCase};
use prost::Message;
use prost_types::FileDescriptorSet;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

/// Call from a contract crate's build.rs, then include!(env!("BRICK_CONTRACT_RS")).
pub fn contract(definition: impl AsRef<Path>, bundle: impl AsRef<Path>) -> Result<()> {
    let definition = definition.as_ref();
    let bundle = bundle.as_ref();
    println!("cargo:rerun-if-changed={}", definition.display());
    println!("cargo:rerun-if-changed={}", bundle.display());
    let output =
        PathBuf::from(std::env::var_os("OUT_DIR").context("brick-build requires Cargo OUT_DIR")?);
    let path = generate(
        &fs::read_to_string(definition)?,
        &brick_contract::load(bundle)?,
        &output,
    )?;
    println!("cargo:rustc-env=BRICK_CONTRACT_RS={}", path.display());
    Ok(())
}

/// Only writes content-addressed generated files under the supplied output directory.
pub fn generate(source: &str, bundle: &Bundle, out: &Path) -> Result<PathBuf> {
    let (frozen, pool) = bundle.validate()?;
    let definition = parse(source)?;
    schema_input(&definition.schema)?;
    ensure!(
        definition.schema == frozen.schema,
        "contract changed: regenerate its descriptor bundle with brick generate before building"
    );
    validate_cases(&definition, &pool)?;
    let service = pool.services().next().context("service")?;
    for message in pool.all_messages() {
        ensure!(
            !matches!(
                message.name().to_upper_camel_case().as_str(),
                "Client" | "Adapter"
            ),
            "typed v0 reserves Client and Adapter for generated helpers"
        );
    }
    // Keep the initial typed surface deliberately narrow; do not guess Rust paths.
    for method in service.methods() {
        ensure!(
            method.input().parent_message().is_none() && method.output().parent_message().is_none(),
            "typed v0 requires top-level request/response messages"
        );
    }
    fs::create_dir_all(out)?;
    let temporary = tempfile::tempdir_in(out)?;
    tonic_prost_build::configure()
        .out_dir(temporary.path())
        .compile_fds(FileDescriptorSet::decode(bundle.descriptor.as_slice())?)?;
    let mut files = fs::read_dir(temporary.path())?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    files.sort();
    ensure!(files.len() == 1, "typed v0 expects one generated package");
    let mut code = fs::read_to_string(&files[0])?;
    let id = contract_id(source)?;
    let service_type = ident(&service.name().to_upper_camel_case());
    let service_module = ident(&service.name().to_snake_case());
    code.push_str(&format!(
        r#"
/// Exact standalone schema revision. Scenario edits do not change this value.
pub const CONTRACT_ID: &str = "{id}";
/// Typed provider bridge for Brick's bounded host and generated shadows.
pub struct Adapter<T>(::std::sync::Arc<T>);
impl<T> Adapter<T> {{ pub fn new(provider:T)->Self {{ Self(::std::sync::Arc::new(provider)) }} }}
impl<T: {service_module}_server::{service_type}> ::brick_rpc::Handler for Adapter<T> {{
fn contract_id(&self) -> Option<&str> {{ Some(CONTRACT_ID) }}
fn call(&self, method: ::prost_reflect::MethodDescriptor, input: ::prost_reflect::DynamicMessage,
        cancel: ::std::sync::Arc<::std::sync::atomic::AtomicBool>) -> ::brick_rpc::Reply {{
    let provider=self.0.clone();
    ::std::boxed::Box::pin(async move {{
        use ::prost::Message;
        match method.name() {{
"#
    ));
    for method in service.methods() {
        let proto = method.name();
        let name = ident(&proto.to_snake_case());
        let input = ident(&method.input().name().to_upper_camel_case());
        code.push_str(&format!(r#""{proto}" => {{
            let value={input}::decode(input.encode_to_vec().as_slice()).map_err(::brick_rpc::internal)?;
            let mut request=::tonic::Request::new(value);request.extensions_mut().insert(cancel);
            let response=provider.{name}(request).await?.into_inner();
            ::prost_reflect::DynamicMessage::decode(method.output(),response.encode_to_vec().as_slice()).map_err(::brick_rpc::internal)
        }},
"#));
    }
    code.push_str("_ => Err(::tonic::Status::unimplemented(\"UNKNOWN_METHOD\")),\n}\n})\n}\n}\n");
    code.push_str(&format!(r#"
/// Preferred typed client: loopback, bounded calls, and contract-revision checks.
pub struct Client {{ inner: {service_module}_client::{service_type}Client<::tonic::transport::Channel> }}
impl Client {{
    pub async fn connect(address: ::std::net::SocketAddr) -> ::anyhow::Result<Self> {{
        ::anyhow::ensure!(address.ip().is_loopback(),"loopback only");
        let channel=::tonic::transport::Channel::from_shared(format!("http://{{address}}"))?
            .connect_timeout(::std::time::Duration::from_secs(2)).timeout(::std::time::Duration::from_secs(15)).connect().await?;
        Ok(Self {{ inner: {service_module}_client::{service_type}Client::new(channel)
            .max_decoding_message_size(1024*1024).max_encoding_message_size(1024*1024) }})
    }}
"#));
    for method in service.methods() {
        let name = ident(&method.name().to_snake_case());
        let input = ident(&method.input().name().to_upper_camel_case());
        let output = ident(&method.output().name().to_upper_camel_case());
        code.push_str(&format!(r#"
    pub async fn {name}(&mut self, value:{input})->::std::result::Result<{output},::tonic::Status> {{
        let mut request=::tonic::Request::new(value);
        request.set_timeout(::std::time::Duration::from_secs(15));
        request.metadata_mut().insert("brick-contract-id",CONTRACT_ID.parse().map_err(::brick_rpc::internal)?);
        let response=self.inner.{name}(request).await?;
        if response.metadata().get("brick-contract-id").and_then(|v|v.to_str().ok())!=Some(CONTRACT_ID) {{
            return Err(::tonic::Status::failed_precondition("CONTRACT_MISMATCH"));
        }}
        Ok(response.into_inner())
    }}
"#));
    }
    code.push_str("}\n");
    let path = out.join(format!("brick-contract-{}.rs", hash(code.as_bytes())));
    if path.exists() {
        ensure!(
            fs::read(&path)? == code.as_bytes(),
            "generated content identity collision"
        );
        return Ok(path);
    }
    let mut file = tempfile::NamedTempFile::new_in(out)?;
    file.write_all(code.as_bytes())?;
    file.as_file().sync_all()?;
    match file.persist_noclobber(&path) {
        Ok(_) => {}
        Err(error) => {
            ensure!(
                path.exists() && fs::read(&path)? == code.as_bytes(),
                "cannot publish generated contract: {}",
                error.error
            );
        }
    }
    Ok(path)
}

// Match prost's identifier escaping, keeping all naming in this codegen library.
fn ident(name: &str) -> String {
    match name {
        "_" | "super" | "self" | "Self" | "extern" | "crate" => format!("{name}_"),
        "as" | "break" | "const" | "continue" | "else" | "enum" | "false" | "fn" | "for" | "if"
        | "impl" | "in" | "let" | "loop" | "match" | "mod" | "move" | "mut" | "pub" | "ref"
        | "return" | "static" | "struct" | "trait" | "true" | "type" | "unsafe" | "use"
        | "where" | "while" | "dyn" | "abstract" | "become" | "box" | "do" | "final" | "macro"
        | "override" | "priv" | "typeof" | "unsized" | "virtual" | "yield" | "async" | "await"
        | "try" | "gen" => format!("r#{name}"),
        _ if name.starts_with(|c: char| c.is_numeric()) => format!("_{name}"),
        _ => name.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Bundle {
        serde_json::from_str(include_str!("../../../fixtures/compiler.bundle.json")).unwrap()
    }
    #[test]
    fn schema_drift_and_imports_fail_before_generation() {
        let bundle = fixture();
        let dir = tempfile::tempdir().unwrap();
        let changed = bundle.source.replacen("Compiler {", "ChangedCompiler {", 1);
        assert!(
            generate(&changed, &bundle, dir.path())
                .unwrap_err()
                .to_string()
                .contains("contract changed")
        );
        let imported = bundle
            .source
            .replacen("syntax =", "import \"other.proto\";\nsyntax =", 1);
        assert!(
            generate(&imported, &bundle, dir.path())
                .unwrap_err()
                .to_string()
                .contains("IMPORTS_FORBIDDEN")
        );
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }
    #[test]
    fn typed_output_is_content_addressed_and_scenario_independent() {
        let bundle = fixture();
        let dir = tempfile::tempdir().unwrap();
        let path = generate(&bundle.source, &bundle, dir.path()).unwrap();
        let before = fs::read(&path).unwrap();
        let no_cases = bundle.source.split("[[cases]]").next().unwrap();
        assert_eq!(generate(no_cases, &bundle, dir.path()).unwrap(), path);
        assert_eq!(fs::read(path).unwrap(), before);
        let source = String::from_utf8(before).unwrap();
        assert!(source.contains("pub trait Compiler"));
        assert!(source.contains("pub struct CompileRequest"));
        assert!(source.contains("impl<T: compiler_server::Compiler>"));
    }
}
