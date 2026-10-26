//! Bootstrap runner adapter; local contract types never import another interface.
use crate::{Mode, Receipt, generate};
use anyhow::{Context, Result, ensure};
use brick_contract::{Bundle, message};
use prost_reflect::{DynamicMessage, MessageDescriptor, SerializeOptions, Value};
use std::{
    net::SocketAddr,
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};
use tonic::Status;

pub const SCHEMA: &str = include_str!("../../../interfaces/run-scenarios.brick.toml");

fn rejection(output: MessageDescriptor, code: &str) -> Result<DynamicMessage, Status> {
    message(
        output,
        &serde_json::json!({"rejection":{"code":code}}).to_string(),
    )
    .map_err(super::internal)
}

pub(crate) async fn invoke(
    input: DynamicMessage,
    output: MessageDescriptor,
    cancel: Arc<AtomicBool>,
) -> Result<DynamicMessage, Status> {
    let mode = input
        .get_field_by_name("mode")
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default();
    let mode = match mode.as_str() {
        "shadow" => Mode::Shadow,
        "interface_check" => Mode::InterfaceCheck,
        "generate_bundle" => Mode::GenerateBundle,
        _ => return rejection(output, "UNSUPPORTED_MODE"),
    };
    let bundle = match generate::decode_bundle(&input) {
        Ok(bundle) => bundle,
        Err(_) => return rejection(output, "INVALID_BUNDLE"),
    };
    let (definition, _) = bundle.validate().map_err(super::internal)?;
    if !(1..=32).contains(&definition.cases.len()) {
        return rejection(output, "CASE_LIMIT");
    }
    let receipt = super::verify_controlled(&bundle, mode, cancel, Some(Duration::from_secs(10)))
        .await
        .map_err(super::internal)?;
    message(output, &serde_json::json!({"receipt":receipt}).to_string()).map_err(super::internal)
}

pub async fn remote(
    contract: &Bundle,
    address: SocketAddr,
    bundle: &Bundle,
    mode: Mode,
) -> Result<Receipt> {
    ensure!(
        address.ip().is_loopback(),
        "bootstrap calls are loopback only"
    );
    let (definition, pool) = contract.validate()?;
    ensure!(
        definition.schema == brick_contract::parse(SCHEMA)?.schema,
        "requires exact RunScenarios contract"
    );
    let input = brick_contract::method(&pool, "Run")?.input();
    let mut request = generate::encode(input, bundle)?;
    request.set_field_by_name(
        "mode",
        Value::String(
            serde_json::to_value(mode)?
                .as_str()
                .context("mode string")?
                .into(),
        ),
    );
    let response = super::call_with_timeout(
        contract,
        address,
        "Run",
        &serde_json::to_string(&request)?,
        Duration::from_secs(30),
    )
    .await?;
    decode(&response, bundle, mode)
}

fn decode(response: &DynamicMessage, bundle: &Bundle, mode: Mode) -> Result<Receipt> {
    ensure!(
        response.has_field_by_name("receipt"),
        "runner rejected request: {}",
        serde_json::to_string(response)?
    );
    let value = response
        .get_field_by_name("receipt")
        .context("receipt field")?;
    let value = value.as_message().context("receipt message")?;
    let mut bytes = Vec::new();
    let options = SerializeOptions::new()
        .use_proto_field_name(true)
        .skip_default_fields(false);
    value.serialize_with_options(&mut serde_json::Serializer::new(&mut bytes), &options)?;
    let receipt: Receipt = serde_json::from_slice(&bytes)?;
    validate_receipt(&receipt, bundle, mode)?;
    Ok(receipt)
}

fn validate_receipt(receipt: &Receipt, bundle: &Bundle, mode: Mode) -> Result<()> {
    let (definition, _) = bundle.validate()?;
    ensure!(
        receipt.bundle_digest == bundle.digest && receipt.mode == mode,
        "receipt refers to a different run"
    );
    let evidence = if mode == Mode::Shadow {
        "shadow_wiring_verified"
    } else {
        "component_contract_verified"
    };
    ensure!(
        receipt.evidence == evidence
            && receipt.cleanup == "server_stopped"
            && receipt.network_sandbox == "not_enforced",
        "inconsistent evidence or cleanup"
    );
    ensure!(
        receipt.executable_digest.len() == 64
            && receipt
                .executable_digest
                .bytes()
                .all(|c| c.is_ascii_hexdigit()),
        "invalid executable identity"
    );
    ensure!(
        receipt.runtime_compiler.is_some()
            == matches!(mode, Mode::InterfaceCheck | Mode::GenerateBundle),
        "inconsistent compiler evidence"
    );
    ensure!(
        receipt.cases_passed <= definition.cases.len(),
        "invalid case count"
    );
    ensure!(
        !receipt.passed || receipt.cases_passed == definition.cases.len(),
        "incomplete passing receipt"
    );
    ensure!(
        receipt.passed == receipt.error.is_none(),
        "inconsistent result/error"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{start, verify};

    #[tokio::test]
    async fn runner_shell_and_real_rejections_conform() {
        let contract = brick_contract::build(SCHEMA).unwrap();
        for mode in [Mode::Shadow, Mode::RunScenarios] {
            let receipt = verify(&contract, mode).await.unwrap();
            assert!(receipt.passed, "{:?}", receipt.error);
        }
    }

    #[tokio::test]
    async fn runner_returns_real_success_failure_and_rejects_bad_requests() {
        let contract = brick_contract::build(SCHEMA).unwrap();
        let mut server = start(
            &contract,
            Mode::RunScenarios,
            "127.0.0.1:0".parse().unwrap(),
        )
        .await
        .unwrap();
        let checker = brick_contract::build(crate::SELF_SCHEMA).unwrap();
        for mode in [Mode::Shadow, Mode::InterfaceCheck] {
            let receipt = remote(&contract, server.address, &checker, mode)
                .await
                .unwrap();
            assert!(receipt.passed, "{:?}", receipt.error);
            assert_eq!(receipt.cases_passed, 3);
            let mut wrong = receipt;
            wrong.bundle_digest = "wrong".into();
            assert!(validate_receipt(&wrong, &checker, mode).is_err());
        }
        let bad = brick_contract::build(&crate::SELF_SCHEMA.replacen(
            "response_json = '{\"valid\":true}'",
            "response_json = '{\"valid\":false}'",
            1,
        ))
        .unwrap();
        let receipt = remote(&contract, server.address, &bad, Mode::InterfaceCheck)
            .await
            .unwrap();
        assert!(!receipt.passed);
        assert_eq!(receipt.cases_passed, 0);
        assert_eq!(receipt.cleanup, "server_stopped");
        assert!(
            remote(&contract, server.address, &contract, Mode::RunScenarios)
                .await
                .unwrap_err()
                .to_string()
                .contains("UNSUPPORTED_MODE")
        );
        let mut empty = brick_contract::parse(crate::SELF_SCHEMA).unwrap();
        empty.cases.clear();
        // Build empty definition without adding a TOML serialization dependency.
        let empty = brick_contract::build(&format!(
            "format_version = 1\nschema = '''{}'''",
            empty.schema
        ))
        .unwrap();
        assert!(
            remote(&contract, server.address, &empty, Mode::Shadow)
                .await
                .unwrap_err()
                .to_string()
                .contains("CASE_LIMIT")
        );
        server.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn generated_runner_executes_generated_checker() {
        let generator = brick_contract::build(generate::SCHEMA).unwrap();
        let mut generating = start(
            &generator,
            Mode::GenerateBundle,
            "127.0.0.1:0".parse().unwrap(),
        )
        .await
        .unwrap();
        let runner = generate::remote(&generator, generating.address, SCHEMA)
            .await
            .unwrap();
        let checker = generate::remote(&generator, generating.address, crate::SELF_SCHEMA)
            .await
            .unwrap();
        let mut running = start(&runner, Mode::RunScenarios, "127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        assert!(
            remote(&runner, running.address, &checker, Mode::InterfaceCheck)
                .await
                .unwrap()
                .passed
        );
        running.shutdown().await.unwrap();
        generating.shutdown().await.unwrap();
    }
    #[tokio::test]
    async fn busy_runner_rejects_overlap_and_shutdown_joins_owned_run() {
        let contract = brick_contract::build(SCHEMA).unwrap();
        let checker = brick_contract::build(crate::SELF_SCHEMA).unwrap();
        let mut server = start(
            &contract,
            Mode::RunScenarios,
            "127.0.0.1:0".parse().unwrap(),
        )
        .await
        .unwrap();
        let address = server.address;
        let request_contract = contract.clone();
        let request_checker = checker.clone();
        let run = tokio::spawn(async move {
            remote(
                &request_contract,
                address,
                &request_checker,
                Mode::InterfaceCheck,
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(2), async {
            while server.runtime.compiler_slots.available_permits() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        run.abort(); // A disconnected client must not release owned-run admission.
        let error = remote(&contract, address, &checker, Mode::InterfaceCheck)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("RUNNER_BUSY"));
        server.shutdown().await.unwrap();
        let result = tokio::time::timeout(Duration::from_secs(2), run)
            .await
            .unwrap();
        assert!(result.is_err());
        assert!(tokio::net::TcpStream::connect(address).await.is_err());
    }

    #[tokio::test]
    async fn execution_deadline_returns_failed_receipt_after_cleanup() {
        let checker = brick_contract::build(crate::SELF_SCHEMA).unwrap();
        let receipt = crate::verify_controlled(
            &checker,
            Mode::InterfaceCheck,
            Arc::new(AtomicBool::new(false)),
            Some(Duration::ZERO),
        )
        .await
        .unwrap();
        assert!(!receipt.passed);
        assert!(receipt.error.unwrap().contains("deadline"));
        assert_eq!(receipt.cleanup, "server_stopped");
    }
}
