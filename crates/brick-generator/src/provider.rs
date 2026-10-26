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
pub const COMPILER_CONTRACT_ID: &str =
    "deff59639a84888a8c1c8e493cac4f04b6236ffab3d43183ab75778f5d100215";

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
            let (_, pool) = compiler.validate().map_err(internal)?;
            let request = message(
                brick_contract::method(&pool, "Compile")
                    .map_err(internal)?
                    .input(),
                &serde_json::json!({"schema":definition.schema}).to_string(),
            )
            .map_err(internal)?;
            let response = brick_rpc::call(&compiler, address, "Compile", &request)
                .await
                .map_err(internal)?;
            if response.has_field_by_name("rejection") {
                let json = serde_json::to_value(&response).map_err(internal)?;
                let code = json["rejection"]["code"]
                    .as_str()
                    .ok_or_else(|| Status::internal("invalid compiler rejection"))?;
                if !matches!(code, "IMPORTS_FORBIDDEN" | "INVALID_SCHEMA") {
                    return Err(Status::internal("unknown compiler rejection"));
                }
                return reject(code);
            }
            if !response.has_field_by_name("compiled") {
                return Err(Status::internal("missing compiler outcome"));
            }
            let field = response
                .get_field_by_name("compiled")
                .ok_or_else(|| Status::internal("compiled field"))?;
            let compiled = field
                .as_message()
                .ok_or_else(|| Status::internal("compiled message"))?;
            let json = serde_json::to_value(compiled).map_err(internal)?;
            if json["schemaDigest"] != hash(definition.schema.as_bytes()) {
                return Err(Status::internal(
                    "compiler returned different schema identity",
                ));
            }
            let descriptor = compiled
                .get_field_by_name("descriptor")
                .and_then(|v| v.as_bytes().map(|b| b.to_vec()))
                .ok_or_else(|| Status::internal("descriptor bytes"))?;
            let identity = brick_contract::CompilerIdentity {
                version: json["compiler"]["version"]
                    .as_str()
                    .ok_or_else(|| Status::internal("compiler version"))?
                    .into(),
                executable_digest: json["compiler"]["executableDigest"]
                    .as_str()
                    .ok_or_else(|| Status::internal("compiler digest"))?
                    .into(),
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
