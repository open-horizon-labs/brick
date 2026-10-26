//! Standalone artifact data and validation. No compiler or provider implementation.
use anyhow::{Context, Result, ensure};
use prost::Message;
use prost_reflect::{DescriptorPool, DynamicMessage, MethodDescriptor};
use prost_types::FileDescriptorSet;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};
pub const FORMAT: u32 = 1;
pub const GENERATOR: &str = concat!("brick-seed/", env!("CARGO_PKG_VERSION"));
pub const MAX_SOURCE_BYTES: usize = 256 * 1024;
#[derive(Debug)]
pub struct SchemaRejection {
    pub code: &'static str,
    pub detail: String,
}
impl std::fmt::Display for SchemaRejection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.detail)
    }
}
impl std::error::Error for SchemaRejection {}

pub fn schema_input(schema: &str) -> Result<()> {
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompilerIdentity {
    pub version: String,
    pub executable_digest: String,
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
    pub fn identity(&self) -> Result<String> {
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

pub fn validate_cases(definition: &Definition, pool: &DescriptorPool) -> Result<()> {
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

/// Exact authored schema identity, independent of tests, compiler and artifacts.
pub fn contract_id(source: &str) -> Result<String> {
    Ok(hash(parse(source)?.schema.as_bytes()))
}
/// Declared cases in order; separate from interface compatibility.
pub fn scenario_id(source: &str) -> Result<String> {
    Ok(hash(&serde_json::to_vec(&parse(source)?.cases)?))
}

pub mod wire;
