//! Real generation adapter. Artifact semantics live in the standalone contract;
//! the seed kernel remains the implementation until later bootstrap migrations.
use anyhow::{Result, ensure};
use brick_contract::{Bundle, message};
use brick_seed::Compiler;
use prost_reflect::{DynamicMessage, MessageDescriptor};
use std::{net::SocketAddr, sync::atomic::AtomicBool};

pub const SCHEMA: &str = include_str!("../../../interfaces/generate-bundle.brick.toml");

pub fn invoke(
    source: &str,
    compiler: &Compiler,
    cancel: &AtomicBool,
    output: MessageDescriptor,
) -> Result<DynamicMessage> {
    if brick_contract::parse(source).is_err() {
        return rejection(output, "INVALID_DEFINITION");
    }
    match brick_seed::build_with_compiler(source, compiler, cancel) {
        Ok(bundle) => encode(output, &bundle),
        Err(error) => match error.downcast_ref::<brick_contract::SchemaRejection>() {
            Some(error) => rejection(output, error.code),
            None => Err(error),
        },
    }
}

fn rejection(output: MessageDescriptor, code: &str) -> Result<DynamicMessage> {
    message(
        output,
        &serde_json::json!({"rejection": {"code": code}}).to_string(),
    )
}

pub(crate) use brick_contract::wire::{decode_bundle, encode_bundle as encode};

/// Validate before publication, including exact correspondence to the requested source.
pub fn decode(response: &DynamicMessage, source: &str) -> Result<Bundle> {
    let bundle = decode_bundle(response)?;
    ensure!(
        bundle.source == source,
        "generator returned bundle for different source"
    );
    Ok(bundle)
}

pub async fn remote(contract: &Bundle, address: SocketAddr, source: &str) -> Result<Bundle> {
    ensure!(
        address.ip().is_loopback(),
        "bootstrap calls are loopback only"
    );
    let (definition, _) = contract.validate()?;
    ensure!(
        definition.schema == brick_contract::parse(SCHEMA)?.schema,
        "requires exact GenerateBundle contract"
    );
    let response = crate::call(
        contract,
        address,
        "Generate",
        &serde_json::json!({"source": source}).to_string(),
    )
    .await?;
    decode(&response, source)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Mode, start, verify};

    #[tokio::test]
    async fn generator_rejections_conform_as_shadow_and_real() {
        let contract = brick_seed::build(SCHEMA).unwrap();
        for mode in [Mode::Shadow, Mode::GenerateBundle] {
            let receipt = verify(&contract, mode).await.unwrap();
            assert!(receipt.passed, "{:?}", receipt.error);
            assert_eq!(receipt.cases_passed, 4);
        }
    }

    #[tokio::test]
    async fn generator_generates_itself_and_the_result_runs() {
        let seed = brick_seed::build(SCHEMA).unwrap();
        let mut server = start(&seed, Mode::GenerateBundle, "127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let generated = remote(&seed, server.address, SCHEMA).await.unwrap();
        assert_eq!(
            serde_json::to_value(&generated).unwrap(),
            serde_json::to_value(&seed).unwrap()
        );
        assert!(verify(&generated, Mode::Shadow).await.unwrap().passed);
        assert!(
            verify(&generated, Mode::GenerateBundle)
                .await
                .unwrap()
                .passed
        );
        // The generated generator also generates another actual component.
        let checker = remote(&generated, server.address, crate::SELF_SCHEMA)
            .await
            .unwrap();
        assert!(verify(&checker, Mode::InterfaceCheck).await.unwrap().passed);
        server.shutdown().await.unwrap();
    }

    #[test]
    fn artifact_adapter_rejects_substitution_and_tampering() {
        let seed = brick_seed::build(SCHEMA).unwrap();
        let (_, pool) = seed.validate().unwrap();
        let output = brick_contract::method(&pool, "Generate").unwrap().output();
        let response = encode(output.clone(), &seed).unwrap();
        assert!(
            decode(&response, "different source")
                .unwrap_err()
                .to_string()
                .contains("different source")
        );
        let mut tampered = seed;
        tampered.rust_bindings.clear();
        assert!(decode(&encode(output, &tampered).unwrap(), SCHEMA).is_err());
    }

    #[tokio::test]
    async fn invalid_cases_are_semantic_rejection_and_wrong_contract_is_refused() {
        let seed = brick_seed::build(SCHEMA).unwrap();
        let mut server = start(&seed, Mode::GenerateBundle, "127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let source = crate::SELF_SCHEMA.replace("method = \"Check\"", "method = \"Missing\"");
        let error = remote(&seed, server.address, &source).await.unwrap_err();
        assert!(error.to_string().contains("INVALID_CASES"));
        let wrong = brick_seed::build(crate::SELF_SCHEMA).unwrap();
        assert!(
            start(&wrong, Mode::GenerateBundle, "127.0.0.1:0".parse().unwrap())
                .await
                .is_err()
        );
        server.shutdown().await.unwrap();
    }
}
