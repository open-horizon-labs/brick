use brick_compiler_contract::{
    CompileRequest, CompileResponse, Compiled, CompilerIdentity, Rejection,
    compile_response::Outcome,
};
use brick_contract::hash;
use brick_rpc::internal;
use std::sync::{Arc, atomic::AtomicBool};
use tonic::{Request, Response, Status};
pub const SCHEMA: &str = include_str!("../../../interfaces/compiler.brick.toml");
pub struct Provider(pub crate::Compiler);

#[tonic::async_trait]
impl brick_compiler_contract::compiler_server::Compiler for Provider {
    async fn compile(
        &self,
        request: Request<CompileRequest>,
    ) -> Result<Response<CompileResponse>, Status> {
        let cancel = request
            .extensions()
            .get::<Arc<AtomicBool>>()
            .cloned()
            .ok_or_else(|| Status::internal("missing Brick lifecycle context"))?;
        let schema = request.into_inner().schema;
        let digest = hash(schema.as_bytes());
        let compiler = self.0.clone();
        let identity = compiler.identity.clone();
        let result = tokio::task::spawn_blocking(move || compiler.compile(&schema, &cancel))
            .await
            .map_err(internal)?;
        let outcome = match result {
            Ok(descriptor) => Outcome::Compiled(Compiled {
                descriptor,
                schema_digest: digest,
                compiler: Some(CompilerIdentity {
                    version: identity.version,
                    executable_digest: identity.executable_digest,
                }),
            }),
            Err(error) => match error.downcast_ref::<brick_contract::SchemaRejection>() {
                Some(error) => Outcome::Rejection(Rejection {
                    code: error.code.into(),
                }),
                None => return Err(internal(error)),
            },
        };
        Ok(Response::new(CompileResponse {
            outcome: Some(outcome),
        }))
    }
}
