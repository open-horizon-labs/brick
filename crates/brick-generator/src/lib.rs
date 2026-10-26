//! Generator implementation. Consumes compiler results; never starts a compiler.
use anyhow::{Context, Result, ensure};
use brick_contract::*;
use prost::Message;
use prost_types::FileDescriptorSet;
use std::{
    collections::BTreeMap,
    fs,
    sync::atomic::{AtomicBool, Ordering},
};

pub fn from_descriptor(
    source: &str,
    descriptor: Vec<u8>,
    compiler: &CompilerIdentity,
    cancel: &AtomicBool,
) -> Result<Bundle> {
    ensure!(!cancel.load(Ordering::Acquire), "generation cancelled");
    let definition = parse(source)?;
    schema_input(&definition.schema)?;
    let pool = validate_descriptor(&descriptor)?;
    validate_cases(&definition, &pool).map_err(|error| SchemaRejection {
        code: "INVALID_CASES",
        detail: error.to_string(),
    })?;
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
        compiler: compiler.clone(),
        source: source.into(),
        descriptor,
        rust_bindings,
        digest: String::new(),
    };
    ensure!(!cancel.load(Ordering::Acquire), "generation cancelled");
    bundle.digest = bundle.identity()?;
    Ok(bundle)
}

pub mod provider;
