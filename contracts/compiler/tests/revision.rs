use brick_compiler_contract::{Adapter, CompileRequest, CompileResponse};
use std::sync::Arc;
struct Stub;
#[tonic::async_trait]
impl brick_compiler_contract::compiler_server::Compiler for Stub {
    async fn compile(
        &self,
        _: tonic::Request<CompileRequest>,
    ) -> Result<tonic::Response<CompileResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented(
            "test does not invoke provider",
        ))
    }
}
#[tokio::test]
async fn host_cannot_advertise_a_different_contract_than_the_typed_provider() {
    let bundle: brick_contract::Bundle =
        serde_json::from_str(include_str!("../../../fixtures/compiler.bundle.json")).unwrap();
    let mut running = brick_rpc::start(
        &bundle,
        Arc::new(Adapter::new(Stub)),
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();
    running.shutdown().await.unwrap();
    let mut changed = bundle;
    changed.source = changed.source.replace(
        "package brick.compiler.v1;",
        "package brick.compiler.v1; // a different accepted revision",
    );
    changed.digest = changed.identity().unwrap();
    changed.validate().unwrap();
    let error = brick_rpc::start(
        &changed,
        Arc::new(Adapter::new(Stub)),
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .err()
    .unwrap();
    assert!(error.to_string().contains("provider contract mismatch"));
}
