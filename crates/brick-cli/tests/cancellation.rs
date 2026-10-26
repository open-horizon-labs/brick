//! Regression for Ctrl-C during real compiler execution, not merely an idle listener.
#![cfg(unix)]
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn ctrl_c_reaps_active_compiler_before_cli_exit() {
    cancellation(
        include_str!("../../../interfaces/interface-check.brick.toml"),
        "interface-check",
    );
}

#[test]
fn ctrl_c_reaps_generator_compiler_before_cli_exit() {
    cancellation(
        include_str!("../../../interfaces/generate-bundle.brick.toml"),
        "generate-bundle",
    );
}

#[test]
fn ctrl_c_reaps_compiler_during_discovery() {
    cancellation_at(brick_runtime::SELF_SCHEMA, "interface-check", true);
}

fn cancellation(source: &str, mode: &str) {
    cancellation_at(source, mode, false);
}

fn cancellation_at(source: &str, mode: &str, during_discovery: bool) {
    let bundle = brick_seed::build(source).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let bundle_path = dir.path().join("bundle.json");
    brick_contract::publish(&bundle, &bundle_path).unwrap();
    let marker = dir.path().join("compiler.pid");
    let compiler = dir.path().join("protoc");
    let quoted_marker = format!("'{}'", marker.to_string_lossy().replace('\'', "'\\''"));
    let version = if during_discovery {
        ""
    } else {
        "if [ \"$1\" = \"--version\" ]; then echo 'libprotoc cancellation-test'; exit 0; fi"
    };
    fs::write(
        &compiler,
        format!("#!/bin/sh\n{version}\necho $$ > {quoted_marker}\nexec /bin/sleep 30\n"),
    )
    .unwrap();
    fs::set_permissions(&compiler, fs::Permissions::from_mode(0o755)).unwrap();
    let mut cli = OwnedChild(
        Command::new(env!("CARGO_BIN_EXE_brick"))
            .args(["verify", bundle_path.to_str().unwrap(), "--mode", mode])
            .env("PATH", dir.path())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let started = Instant::now();
    while !marker.exists() && started.elapsed() < Duration::from_secs(5) {
        assert!(
            cli.0.try_wait().unwrap().is_none(),
            "CLI exited before compiler started"
        );
        thread::sleep(Duration::from_millis(10));
    }
    let compiler_pid = fs::read_to_string(&marker).expect("compiler did not start");
    assert!(
        Command::new("/bin/kill")
            .args(["-INT", &cli.0.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let interrupted = Instant::now();
    let status = loop {
        if let Some(status) = cli.0.try_wait().unwrap() {
            break Some(status);
        }
        if interrupted.elapsed() > Duration::from_secs(4) {
            break None;
        }
        thread::sleep(Duration::from_millis(10));
    };
    let alive = Command::new("/bin/kill")
        .args(["-0", compiler_pid.trim()])
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success();
    if alive {
        // Clean up even when testing a regressed executable.
        let _ = Command::new("/bin/kill")
            .args(["-KILL", compiler_pid.trim()])
            .status();
    }
    assert!(status.is_some(), "CLI did not finish cancellation promptly");
    assert!(!status.unwrap().success());
    assert!(!alive, "compiler survived CLI cancellation");
}
