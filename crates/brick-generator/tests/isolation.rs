//! Frozen inputs only. This test target must neither build nor execute protoc.
use brick_contract::{Bundle, contract_id, message, method, parse, scenario_id};
use brick_generator::provider::{Generator, SCHEMA};
use brick_rpc::{Shadow, start};
use std::{net::SocketAddr, sync::Arc};
const ECHO: &str = include_str!("../../../interfaces/echo.brick.toml");
fn compiler() -> Bundle {
    serde_json::from_str(include_str!(
        "../../../fixtures/compiler-shadow.bundle.json"
    ))
    .unwrap()
}
fn generator() -> Bundle {
    serde_json::from_str(include_str!(
        "../../../fixtures/generate-bundle.bundle.json"
    ))
    .unwrap()
}
fn request(bundle: &Bundle) -> prost_reflect::DynamicMessage {
    let (_, pool) = bundle.validate().unwrap();
    message(
        method(&pool, "Generate").unwrap().input(),
        &serde_json::json!({"source":ECHO}).to_string(),
    )
    .unwrap()
}
fn scenario(mut bundle: Bundle, response: serde_json::Value) -> Bundle {
    let mut definition = parse(&bundle.source).unwrap();
    definition.cases[0].response_json = response.to_string();
    bundle.source = toml::to_string(&definition).unwrap();
    bundle.digest = bundle.identity().unwrap();
    bundle.validate().unwrap();
    bundle
}
async fn run(compiler: Bundle) -> (Result<prost_reflect::DynamicMessage, anyhow::Error>, Bundle) {
    let mut shadow = start(
        &compiler,
        Arc::new(Shadow::new(&compiler).unwrap()),
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();
    let generator_bundle = generator();
    let provider = Generator::new(
        compiler.clone(),
        shadow.address,
        &contract_id(&compiler.source).unwrap(),
    )
    .unwrap();
    let mut server = start(
        &generator_bundle,
        Arc::new(provider),
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();
    let result = brick_rpc::call(
        &generator_bundle,
        server.address,
        "Generate",
        &request(&generator_bundle),
    )
    .await;
    shadow.assert_complete().unwrap();
    server.shutdown().await.unwrap();
    shadow.shutdown().await.unwrap();
    (result, generator_bundle)
}
#[tokio::test]
async fn real_generation_against_frozen_compiler_shadow() {
    let (response, _) = run(compiler()).await;
    let bundle = brick_contract::wire::decode_bundle(&response.unwrap()).unwrap();
    assert_eq!(bundle.source, ECHO);
    assert_eq!(bundle.compiler.version, "fixture/compiler-v1");
    let rust = &bundle.rust_bindings["example.echo.v1.rs"];
    assert!(rust.contains("pub struct Request"));
    assert!(rust.contains("pub struct Reply"));
    assert!(rust.contains("pub text: ::prost::alloc::string::String"));
    let mut output = start(
        &bundle,
        Arc::new(Shadow::new(&bundle).unwrap()),
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();
    let (_, pool) = bundle.validate().unwrap();
    let req = message(
        method(&pool, "Say").unwrap().input(),
        "{\"text\":\"hello\"}",
    )
    .unwrap();
    let reply = brick_rpc::call(&bundle, output.address, "Say", &req)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(reply).unwrap()["text"],
        "hello from a generated shadow"
    );
    output.assert_complete().unwrap();
    output.shutdown().await.unwrap();
}
#[tokio::test]
async fn dependency_failure_and_corruption_are_not_success() {
    let base = compiler();
    let definition = parse(&base.source).unwrap();
    let original: serde_json::Value =
        serde_json::from_str(&definition.cases[0].response_json).unwrap();
    for field in ["descriptor", "schema_digest"] {
        let mut response = original.clone();
        response["compiled"][field] = serde_json::json!(if field == "descriptor" {
            "AA=="
        } else {
            "wrong"
        });
        assert!(run(scenario(base.clone(), response)).await.0.is_err());
    }
    let response = run(scenario(
        base,
        serde_json::json!({"rejection":{"code":"INVALID_SCHEMA"}}),
    ))
    .await
    .0
    .unwrap();
    assert_eq!(
        serde_json::to_value(response).unwrap()["rejection"]["code"],
        "INVALID_SCHEMA"
    );
}
#[tokio::test]
async fn unavailable_dependency_fails_and_peer_revision_mismatch_is_explicit() {
    let compiler = compiler();
    let generator_bundle = generator();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let unavailable = listener.local_addr().unwrap();
    drop(listener);
    let provider = Generator::new(
        compiler.clone(),
        unavailable,
        &contract_id(&compiler.source).unwrap(),
    )
    .unwrap();
    let mut server = start(
        &generator_bundle,
        Arc::new(provider),
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();
    assert!(
        brick_rpc::call(
            &generator_bundle,
            server.address,
            "Generate",
            &request(&generator_bundle)
        )
        .await
        .is_err()
    );
    server.shutdown().await.unwrap();
    let mut changed = compiler.clone();
    let mut definition = parse(&changed.source).unwrap();
    definition.schema.push_str("\n// next explicit revision\n");
    changed.source = toml::to_string(&definition).unwrap();
    changed.digest = changed.identity().unwrap();
    assert!(
        Generator::new(
            changed.clone(),
            unavailable,
            &contract_id(&compiler.source).unwrap()
        )
        .is_err()
    );
    let mut peer = start(
        &changed,
        Arc::new(Shadow::new(&changed).unwrap()),
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();
    let (_, pool) = compiler.validate().unwrap();
    let req = message(
        method(&pool, "Compile").unwrap().input(),
        &parse(&compiler.source).unwrap().cases[0].request_json,
    )
    .unwrap();
    let error = brick_rpc::call(&compiler, peer.address, "Compile", &req)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("CONTRACT_MISMATCH"));
    peer.shutdown().await.unwrap();
}
#[test]
fn test_edits_do_not_change_contract_identity() {
    let base = compiler();
    let changed = scenario(
        base.clone(),
        serde_json::json!({"rejection":{"code":"INVALID_SCHEMA"}}),
    );
    assert_eq!(
        contract_id(&base.source).unwrap(),
        contract_id(&changed.source).unwrap()
    );
    assert_ne!(
        scenario_id(&base.source).unwrap(),
        scenario_id(&changed.source).unwrap()
    );
    assert_ne!(base.digest, changed.digest);
    assert_eq!(
        contract_id(&generator().source).unwrap(),
        contract_id(SCHEMA).unwrap()
    );
    assert!(Generator::new(base, "127.0.0.1:1".parse::<SocketAddr>().unwrap(), "wrong").is_err());
}
