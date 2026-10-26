//! The bootstrap contract kernel. No transport or application implementation dependency.
use anyhow::{Context, Result, bail, ensure};
use prost::Message;
use prost_reflect::{DescriptorPool, DynamicMessage, MethodDescriptor};
use prost_types::FileDescriptorSet;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Output, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

pub const FORMAT: u32 = 1;
pub const GENERATOR: &str = concat!("brick-seed/", env!("CARGO_PKG_VERSION"));
pub const MAX_SOURCE_BYTES: usize = 256 * 1024;

fn compiler_path() -> Result<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|path| path.join("protoc"))
        .find(|path| path.is_file())
        .context("protoc is required on PATH")?
        .canonicalize()
        .context("cannot resolve protoc")
}

#[derive(Debug)]
pub struct SchemaRejection {
    pub code: &'static str,
    detail: String,
}
impl std::fmt::Display for SchemaRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.detail)
    }
}
impl std::error::Error for SchemaRejection {}

fn schema_input(schema: &str) -> Result<()> {
    if schema.len() > MAX_SOURCE_BYTES {
        return Err(SchemaRejection {
            code: "INVALID_SCHEMA",
            detail: "schema exceeds 256 KiB".into(),
        }
        .into());
    }
    if has_import(schema) {
        return Err(SchemaRejection {
            code: "IMPORTS_FORBIDDEN",
            detail: "interfaces must be standalone".into(),
        }
        .into());
    }
    Ok(())
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn bounded(mut command: Command, cancel: &AtomicBool, deadline: Duration) -> Result<Output> {
    ensure!(!cancel.load(Ordering::Acquire), "compiler cancelled");
    let output = tempfile::tempdir()?;
    let stdout = output.path().join("stdout");
    let stderr = output.path().join("stderr");
    let mut child = ChildGuard(
        command
            .stdin(Stdio::null())
            .stdout(fs::File::create(&stdout)?)
            .stderr(fs::File::create(&stderr)?)
            .spawn()
            .context("could not start protoc")?,
    );
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.0.try_wait()? {
            break status;
        }
        ensure!(!cancel.load(Ordering::Acquire), "compiler cancelled");
        ensure!(started.elapsed() < deadline, "protoc exceeded deadline");
        thread::sleep(Duration::from_millis(10));
    };
    Ok(Output {
        status,
        stdout: fs::read(stdout)?,
        stderr: fs::read(stderr)?,
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompilerIdentity {
    pub version: String,
    pub executable_digest: String,
}

#[derive(Clone)]
pub struct Compiler {
    path: PathBuf,
    pub identity: CompilerIdentity,
}

impl Compiler {
    pub fn discover() -> Result<Self> {
        let path = compiler_path()?;
        let executable_digest = hash(&fs::read(&path)?);
        let mut command = Command::new(&path);
        command.env_clear().arg("--version");
        let version = bounded(command, &AtomicBool::new(false), Duration::from_secs(10))?;
        ensure!(version.status.success(), "could not identify protoc");
        Ok(Self {
            path,
            identity: CompilerIdentity {
                version: String::from_utf8(version.stdout)?.trim().into(),
                executable_digest,
            },
        })
    }

    pub fn compile(&self, schema: &str, cancel: &AtomicBool) -> Result<Vec<u8>> {
        schema_input(schema)?;
        ensure!(
            hash(&fs::read(&self.path)?) == self.identity.executable_digest,
            "compiler changed during run"
        );
        let dir = tempfile::tempdir()?;
        fs::write(dir.path().join("interface.proto"), schema)?;
        let mut command = Command::new(&self.path);
        command.env_clear().current_dir(dir.path()).args([
            "--proto_path=.",
            "--descriptor_set_out=interface.pb",
            "interface.proto",
        ]);
        let output = bounded(command, cancel, Duration::from_secs(10))?;
        if !output.status.success() {
            let diagnostics = String::from_utf8_lossy(&output.stderr);
            // Source-located parser diagnostics are semantic rejection. Termination,
            // output-file failures, and other compiler failures remain infrastructure errors.
            if output.status.code() == Some(1)
                && !diagnostics.trim().is_empty()
                && diagnostics
                    .lines()
                    .all(|line| line.starts_with("interface.proto:"))
            {
                return Err(SchemaRejection {
                    code: "INVALID_SCHEMA",
                    detail: diagnostics.into(),
                }
                .into());
            }
            bail!(
                "compiler execution failed ({}): {}",
                output.status,
                diagnostics
            );
        }
        let bytes = fs::read(dir.path().join("interface.pb"))?;
        FileDescriptorSet::decode(bytes.as_slice())
            .context("compiler emitted invalid descriptor bytes")?;
        validate_descriptor(&bytes).map_err(|error| SchemaRejection {
            code: "INVALID_SCHEMA",
            detail: error.to_string(),
        })?;
        ensure!(
            hash(&fs::read(&self.path)?) == self.identity.executable_digest,
            "compiler changed during run"
        );
        Ok(bytes)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Definition {
    pub format_version: u32,
    pub schema: String,
    #[serde(default)]
    pub cases: Vec<Case>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    pub name: String,
    pub method: String,
    pub request_json: String,
    pub response_json: String,
}

/// Immutable portable output; no absolute build paths in its identity.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bundle {
    pub format_version: u32,
    pub generator: String,
    pub compiler: CompilerIdentity,
    pub source: String,
    pub descriptor: Vec<u8>,
    pub rust_bindings: BTreeMap<String, String>,
    pub digest: String,
}

pub fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

impl Bundle {
    fn identity(&self) -> Result<String> {
        let mut unsigned = self.clone();
        unsigned.digest.clear();
        Ok(hash(&serde_json::to_vec(&unsigned)?))
    }

    pub fn validate(&self) -> Result<(Definition, DescriptorPool)> {
        ensure!(self.format_version == FORMAT, "unsupported bundle format");
        ensure!(self.generator == GENERATOR, "unsupported generator version");
        ensure!(self.digest == self.identity()?, "bundle digest mismatch");
        let definition = parse(&self.source)?;
        ensure!(!has_import(&definition.schema), "IMPORTS_FORBIDDEN");
        let pool = validate_descriptor(&self.descriptor)?;
        validate_cases(&definition, &pool)?;
        Ok((definition, pool))
    }
}

pub fn parse(source: &str) -> Result<Definition> {
    ensure!(
        source.len() <= MAX_SOURCE_BYTES,
        "definition exceeds 256 KiB"
    );
    let definition: Definition = toml::from_str(source).context("invalid Brick definition")?;
    ensure!(
        definition.format_version == FORMAT,
        "unsupported definition format"
    );
    Ok(definition)
}

/// A lexical preflight, not a protobuf parser. Ignore comments and quoted text;
/// the actual parser remains protoc. Every import token is rejected before IO.
pub fn has_import(schema: &str) -> bool {
    let b = schema.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i..].starts_with(b"//") {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if b[i..].starts_with(b"/*") {
            i += 2;
            while i < b.len() && !b[i..].starts_with(b"*/") {
                i += 1;
            }
            i = (i + 2).min(b.len());
        } else if b[i] == b'"' || b[i] == b'\'' {
            let quote = b[i];
            i += 1;
            while i < b.len() {
                if b[i] == b'\\' {
                    i = (i + 2).min(b.len());
                } else if b[i] == quote {
                    i += 1;
                    break;
                } else {
                    i += 1;
                }
            }
        } else if b[i].is_ascii_alphabetic() || b[i] == b'_' {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            if &b[start..i] == b"import" {
                return true;
            }
        } else {
            i += 1;
        }
    }
    false
}

pub fn compile_schema(schema: &str) -> Result<Vec<u8>> {
    schema_input(schema)?;
    Compiler::discover()?.compile(schema, &AtomicBool::new(false))
}

pub fn validate_descriptor(bytes: &[u8]) -> Result<DescriptorPool> {
    let fds = FileDescriptorSet::decode(bytes)?;
    ensure!(
        fds.file.len() == 1,
        "exactly one standalone file is required"
    );
    let file = &fds.file[0];
    ensure!(file.dependency.is_empty(), "IMPORTS_FORBIDDEN");
    ensure!(
        file.syntax.as_deref() == Some("proto3"),
        "v0 requires proto3"
    );
    ensure!(
        !file.package.as_deref().unwrap_or_default().is_empty(),
        "package is required"
    );
    ensure!(
        file.service.len() == 1,
        "v0 requires exactly one service per interface"
    );
    ensure!(
        !file.service[0].method.is_empty(),
        "service must have a method"
    );
    for method in &file.service[0].method {
        ensure!(
            !method.client_streaming.unwrap_or(false) && !method.server_streaming.unwrap_or(false),
            "v0 supports unary methods only"
        );
    }
    Ok(DescriptorPool::from_file_descriptor_set(fds)?)
}

pub fn method(pool: &DescriptorPool, name: &str) -> Result<MethodDescriptor> {
    pool.services()
        .flat_map(|s| s.methods().collect::<Vec<_>>())
        .find(|m| {
            m.name() == name || format!("/{}/{}", m.parent_service().full_name(), m.name()) == name
        })
        .with_context(|| format!("unknown method: {name}"))
}

pub fn message(desc: prost_reflect::MessageDescriptor, json: &str) -> Result<DynamicMessage> {
    let mut de = serde_json::Deserializer::from_str(json);
    let value = DynamicMessage::deserialize(desc, &mut de)?;
    de.end()?;
    Ok(value)
}

fn validate_cases(definition: &Definition, pool: &DescriptorPool) -> Result<()> {
    let mut names = std::collections::HashSet::new();
    for case in &definition.cases {
        ensure!(
            !case.name.is_empty() && names.insert(&case.name),
            "case names must be nonempty and unique"
        );
        let method = method(pool, &case.method)?;
        message(method.input(), &case.request_json)
            .with_context(|| format!("{} request", case.name))?;
        message(method.output(), &case.response_json)
            .with_context(|| format!("{} response", case.name))?;
    }
    Ok(())
}

pub fn build(source: &str) -> Result<Bundle> {
    let definition = parse(source)?;
    schema_input(&definition.schema)?;
    let compiler = Compiler::discover()?;
    let descriptor = compiler.compile(&definition.schema, &AtomicBool::new(false))?;
    let pool = validate_descriptor(&descriptor)?;
    validate_cases(&definition, &pool)?;
    let dir = tempfile::tempdir()?;
    prost_build::Config::new()
        .out_dir(dir.path())
        .compile_fds(FileDescriptorSet::decode(descriptor.as_slice())?)?;
    let mut rust_bindings = BTreeMap::new();
    for entry in fs::read_dir(dir.path())? {
        let path = entry?.path();
        let filename = path
            .file_name()
            .context("generated filename")?
            .to_str()
            .context("UTF-8 filename")?;
        rust_bindings.insert(filename.to_owned(), fs::read_to_string(path)?);
    }
    let mut bundle = Bundle {
        format_version: FORMAT,
        generator: GENERATOR.into(),
        compiler: compiler.identity,
        source: source.into(),
        descriptor,
        rust_bindings,
        digest: String::new(),
    };
    bundle.digest = bundle.identity()?;
    Ok(bundle)
}

/// Publish a single self-contained bundle without clobbering any existing path.
pub fn publish(bundle: &Bundle, output: &Path) -> Result<()> {
    bundle.validate()?;
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    use std::io::Write;
    temporary.write_all(&serde_json::to_vec_pretty(bundle)?)?;
    temporary.as_file().sync_all()?;
    temporary
        .persist_noclobber(output)
        .map_err(|e| e.error)
        .with_context(|| format!("refusing to replace existing output {}", output.display()))?;
    Ok(())
}

pub fn load(path: &Path) -> Result<Bundle> {
    ensure!(
        fs::metadata(path)?.len() <= 8 * 1024 * 1024,
        "bundle exceeds 8 MiB"
    );
    let bundle: Bundle = serde_json::from_slice(&fs::read(path)?)?;
    bundle.validate()?;
    Ok(bundle)
}

pub fn default_output(bundle: &Bundle) -> PathBuf {
    PathBuf::from(".brick").join(format!("{}.json", bundle.digest))
}

#[cfg(test)]
mod tests {
    use super::*;
    const ECHO: &str = include_str!("../../../interfaces/echo.brick.toml");

    #[test]
    fn execution_failures_are_not_schema_rejections() {
        let compiler = Compiler {
            path: PathBuf::from("/definitely-missing-brick-compiler"),
            identity: CompilerIdentity {
                version: "missing".into(),
                executable_digest: "missing".into(),
            },
        };
        let schema = parse(ECHO).unwrap().schema;
        let error = compiler
            .compile(&schema, &AtomicBool::new(false))
            .unwrap_err();
        assert!(error.downcast_ref::<SchemaRejection>().is_none());
        let compiler = Compiler::discover().unwrap();
        let error = compiler
            .compile("not protobuf", &AtomicBool::new(false))
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<SchemaRejection>().unwrap().code,
            "INVALID_SCHEMA"
        );
    }

    #[cfg(unix)]
    #[test]
    fn subprocess_timeout_reaps_child() {
        let mut command = Command::new("/bin/sleep");
        command.arg("30");
        let started = Instant::now();
        let error =
            bounded(command, &AtomicBool::new(false), Duration::from_millis(30)).unwrap_err();
        assert!(error.to_string().contains("deadline"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn imports_cannot_hide_behind_comments_or_whitespace() {
        for source in [
            "import \"x.proto\";",
            "/* comment */ import public \"x\";",
            "import\n\"x\";",
        ] {
            assert!(has_import(source));
            assert!(
                compile_schema(source)
                    .unwrap_err()
                    .to_string()
                    .contains("IMPORTS_FORBIDDEN")
            );
        }
        assert!(!has_import(
            "// import x\nmessage Important { string import_name = 1; }"
        ));
        assert!(!has_import("option text = \"import x\"; /* import y */"));
    }

    #[test]
    fn generation_is_deterministic_and_publishes_without_overwrite() {
        let a = build(ECHO).unwrap();
        let b = build(ECHO).unwrap();
        assert_eq!(a.digest, b.digest);
        assert!(!a.rust_bindings.is_empty());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bundle.json");
        publish(&a, &path).unwrap();
        assert!(publish(&b, &path).is_err());
        assert_eq!(load(&path).unwrap().digest, a.digest);
        let mut altered = a;
        altered.source.push('\n');
        assert!(altered.validate().is_err());
    }

    #[test]
    fn invalid_scenarios_and_streams_are_rejected() {
        assert!(build(&ECHO.replace("hello\"}", "hello\",\"extra\":true}")).is_err());
        assert!(build(&ECHO.replace("rpc Say(Request)", "rpc Say(stream Request)")).is_err());
        assert!(build(&ECHO.replace("method = \"Say\"", "method = \"Missing\"")).is_err());
    }
}
