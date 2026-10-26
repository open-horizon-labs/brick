use anyhow::{Result, ensure};
use brick_contract::{
    Bundle, contract_id, hash, message, parse, schema_input, wire::encode_bundle,
};
use brick_rpc::{Handler, Reply, internal};
use prost_reflect::{DynamicMessage, MethodDescriptor};
use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tonic::Status;
pub const SCHEMA: &str = include_str!("../../../interfaces/generate-bundle.brick.toml");
// Accepted dependency revision; changing it is an explicit compatibility decision.
pub const COMPILER_CONTRACT_ID: &str = brick_compiler_contract::CONTRACT_ID;

pub struct Generator {
    compiler: Bundle,
    address: SocketAddr,
}
impl Generator {
    pub fn new(compiler: Bundle, address: SocketAddr, pin: &str) -> Result<Self> {
        compiler.validate()?;
        ensure!(address.ip().is_loopback(), "loopback only");
        ensure!(
            contract_id(&compiler.source)? == pin && pin == COMPILER_CONTRACT_ID,
            "compiler contract pin mismatch"
        );
        Ok(Self { compiler, address })
    }
}
impl Handler for Generator {
    fn call(
        &self,
        method: MethodDescriptor,
        input: DynamicMessage,
        cancel: Arc<AtomicBool>,
    ) -> Reply {
        let compiler = self.compiler.clone();
        let address = self.address;
        Box::pin(async move {
            let source = input
                .get_field_by_name("source")
                .and_then(|v| v.as_str().map(str::to_owned))
                .ok_or_else(|| Status::invalid_argument("missing source"))?;
            let reject = |code: &str| {
                message(
                    method.output(),
                    &serde_json::json!({"rejection":{"code":code}}).to_string(),
                )
                .map_err(internal)
            };
            let definition = match parse(&source) {
                Ok(v) => v,
                Err(_) => return reject("INVALID_DEFINITION"),
            };
            if let Err(e) = schema_input(&definition.schema) {
                return reject(
                    e.downcast_ref::<brick_contract::SchemaRejection>()
                        .ok_or_else(|| internal(&e))?
                        .code,
                );
            }
            compiler.validate().map_err(internal)?;
            let mut client = brick_compiler_contract::Client::connect(address)
                .await
                .map_err(internal)?;
            let response = client
                .compile(brick_compiler_contract::CompileRequest {
                    schema: definition.schema.clone(),
                })
                .await?;
            let compiled = match response.outcome {
                Some(brick_compiler_contract::compile_response::Outcome::Rejection(error)) => {
                    if !matches!(error.code.as_str(), "IMPORTS_FORBIDDEN" | "INVALID_SCHEMA") {
                        return Err(Status::internal("unknown compiler rejection"));
                    }
                    return reject(&error.code);
                }
                Some(brick_compiler_contract::compile_response::Outcome::Compiled(compiled)) => {
                    compiled
                }
                None => return Err(Status::internal("missing compiler outcome")),
            };
            if compiled.schema_digest != hash(definition.schema.as_bytes()) {
                return Err(Status::internal(
                    "compiler returned different schema identity",
                ));
            }
            let descriptor = compiled.descriptor;
            let identity = compiled
                .compiler
                .ok_or_else(|| Status::internal("missing compiler identity"))?;
            let identity = brick_contract::CompilerIdentity {
                version: identity.version,
                executable_digest: identity.executable_digest,
            };
            if identity.executable_digest.len() != 64
                || !identity
                    .executable_digest
                    .bytes()
                    .all(|v| v.is_ascii_hexdigit())
            {
                return Err(Status::internal("invalid compiler identity"));
            }
            if cancel.load(Ordering::Acquire) {
                return Err(Status::cancelled("generation cancelled"));
            }
            // CPU/file work stays off the transport executor; supervisor retains admission.
            let result = tokio::task::spawn_blocking(move || {
                crate::from_descriptor(&source, descriptor, &identity, &cancel)
            })
            .await
            .map_err(internal)?;
            match result {
                Ok(bundle) => encode_bundle(method.output(), &bundle).map_err(internal),
                Err(e) => match e.downcast_ref::<brick_contract::SchemaRejection>() {
                    Some(e) => reject(e.code),
                    None => Err(internal(e)),
                },
            }
        })
    }
}
