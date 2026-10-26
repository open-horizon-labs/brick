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
