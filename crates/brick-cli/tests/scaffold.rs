use std::{fs, process::Command};
#[test]
fn module_scaffold_is_complete_and_refuses_overwrite_and_unsafe_names() {
    let root = tempfile::tempdir().unwrap();
    let output = root.path().join("demo");
    let run = |name: &str| {
        Command::new(env!("CARGO_BIN_EXE_brick"))
            .args(["new", "module", name, "--path"])
            .arg(&output)
            .output()
            .unwrap()
    };
    let rejected = run("../escape");
    assert!(!rejected.status.success());
    assert!(!output.exists());
    let created = run("demo");
    assert!(
        created.status.success(),
        "{}",
        String::from_utf8_lossy(&created.stderr)
    );
    assert!(!output.join(".brick-incomplete").exists());
    assert!(output.join("contract/build.rs").is_file());
    assert!(output.join("implementation/tests/conformance.rs").is_file());
    let provider = output.join("implementation/src/lib.rs");
    fs::write(&provider, "authored work").unwrap();
    assert!(!run("demo").status.success());
    assert_eq!(fs::read_to_string(provider).unwrap(), "authored work");
}
