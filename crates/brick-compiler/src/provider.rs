use brick_contract::{hash, message};
use brick_rpc::{Handler, Reply, internal};
use prost_reflect::{DynamicMessage, MethodDescriptor, Value};
use std::sync::{Arc, atomic::AtomicBool};
use tonic::Status;
pub const SCHEMA: &str = include_str!("../../../interfaces/compiler.brick.toml");
pub struct Provider(pub crate::Compiler);
impl Handler for Provider {
    fn call(
        &self,
        method: MethodDescriptor,
        input: DynamicMessage,
        cancel: Arc<AtomicBool>,
    ) -> Reply {
        let compiler = self.0.clone();
        Box::pin(async move {
            let schema = input
                .get_field_by_name("schema")
                .and_then(|v| v.as_str().map(str::to_owned))
                .ok_or_else(|| Status::invalid_argument("missing schema"))?;
            let digest = hash(schema.as_bytes());
            let identity = compiler.identity.clone();
            let result = tokio::task::spawn_blocking(move || compiler.compile(&schema, &cancel))
                .await
                .map_err(internal)?;
            match result {
                Ok(bytes) => {
                    let mut response=message(method.output(),&serde_json::json!({"compiled":{"schema_digest":digest,"compiler":identity}}).to_string()).map_err(internal)?;
                    if let Some(Value::Message(compiled)) =
                        response.get_field_by_name_mut("compiled")
                    {
                        compiled.set_field_by_name("descriptor", Value::Bytes(bytes.into()));
                    }
                    Ok(response)
                }
                Err(e) => match e.downcast_ref::<brick_contract::SchemaRejection>() {
                    Some(e) => message(
                        method.output(),
                        &serde_json::json!({"rejection":{"code":e.code}}).to_string(),
                    )
                    .map_err(internal),
                    None => Err(internal(e)),
                },
            }
        })
    }
}
