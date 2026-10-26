use std::process::Command;

#[tokio::test]
async fn cli_generates_over_its_boundary_without_overwriting() {
    let contract = brick_contract::build(brick_runtime::generate::SCHEMA).unwrap();
    let mut server = brick_runtime::start(
        &contract,
        brick_runtime::Mode::GenerateBundle,
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();
    let address = server.address;
    tokio::task::spawn_blocking(move || {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.toml");
        let contract_path = dir.path().join("generator.json");
        let output = dir.path().join("result.json");
        std::fs::write(&source, brick_runtime::generate::SCHEMA).unwrap();
        brick_contract::publish(&contract, &contract_path).unwrap();
        let run = || {
            Command::new(env!("CARGO_BIN_EXE_brick"))
                .arg("generate")
                .arg(&source)
                .arg("--via")
                .arg(address.to_string())
                .arg("--contract")
                .arg(&contract_path)
                .arg("--output")
                .arg(&output)
                .output()
                .unwrap()
        };
        let result = run();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(
            brick_contract::load(&output).unwrap().digest,
            contract.digest
        );
        let bytes = std::fs::read(&output).unwrap();
        assert!(!run().status.success());
        assert_eq!(std::fs::read(&output).unwrap(), bytes);
    })
    .await
    .unwrap();
    server.shutdown().await.unwrap();
}

#[tokio::test]
async fn cli_verifies_via_runner_and_preserves_failure_exit_status() {
    let contract = brick_contract::build(brick_runtime::scenarios::SCHEMA).unwrap();
    let mut server = brick_runtime::start(
        &contract,
        brick_runtime::Mode::RunScenarios,
        "127.0.0.1:0".parse().unwrap(),
    )
    .await
    .unwrap();
    let address = server.address;
    tokio::task::spawn_blocking(move || {
        let dir = tempfile::tempdir().unwrap();
        let contract_path = dir.path().join("runner.json");
        brick_contract::publish(&contract, &contract_path).unwrap();
        for (name, source, passed) in [
            ("good", brick_runtime::SELF_SCHEMA.to_owned(), true),
            (
                "bad",
                brick_runtime::SELF_SCHEMA.replacen(
                    "response_json = '{\"valid\":true}'",
                    "response_json = '{\"valid\":false}'",
                    1,
                ),
                false,
            ),
        ] {
            let bundle = brick_contract::build(&source).unwrap();
            let path = dir.path().join(format!("{name}.json"));
            brick_contract::publish(&bundle, &path).unwrap();
            let result = Command::new(env!("CARGO_BIN_EXE_brick"))
                .arg("verify")
                .arg(path)
                .args(["--mode", "interface-check", "--via", &address.to_string()])
                .arg("--contract")
                .arg(&contract_path)
                .output()
                .unwrap();
            assert_eq!(
                result.status.success(),
                passed,
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            let receipt: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
            assert_eq!(receipt["passed"], passed);
            assert_eq!(receipt["bundle_digest"], bundle.digest);
        }
    })
    .await
    .unwrap();
    server.shutdown().await.unwrap();
}
