//! Real protoc adapter, excluded from generator builds.
use anyhow::{Context, Result, bail, ensure};
use brick_contract::{CompilerIdentity, SchemaRejection, hash, schema_input, validate_descriptor};
use prost::Message;
use prost_types::FileDescriptorSet;
use std::{
    fs,
    path::PathBuf,
    process::{Child, Command, Output, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};
fn compiler_path() -> Result<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|path| path.join("protoc"))
        .find(|path| path.is_file())
        .context("protoc is required on PATH")?
        .canonicalize()
        .context("cannot resolve protoc")
}

struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn bounded(mut command: Command, cancel: &AtomicBool, deadline: Duration) -> Result<Output> {
    ensure!(!cancel.load(Ordering::Acquire), "compiler cancelled");
    let output = tempfile::tempdir()?;
    let stdout = output.path().join("stdout");
    let stderr = output.path().join("stderr");
    let mut child = ChildGuard(
        command
            .stdin(Stdio::null())
            .stdout(fs::File::create(&stdout)?)
            .stderr(fs::File::create(&stderr)?)
            .spawn()
            .context("could not start protoc")?,
    );
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.0.try_wait()? {
            break status;
        }
        ensure!(!cancel.load(Ordering::Acquire), "compiler cancelled");
        ensure!(started.elapsed() < deadline, "protoc exceeded deadline");
        thread::sleep(Duration::from_millis(10));
    };
    Ok(Output {
        status,
        stdout: fs::read(stdout)?,
        stderr: fs::read(stderr)?,
    })
}

#[derive(Clone)]
pub struct Compiler {
    path: PathBuf,
    pub identity: CompilerIdentity,
}

impl Compiler {
    pub fn discover() -> Result<Self> {
        Self::discover_with_cancel(&AtomicBool::new(false))
    }

    pub fn discover_with_cancel(cancel: &AtomicBool) -> Result<Self> {
        let path = compiler_path()?;
        let executable_digest = hash(&fs::read(&path)?);
        let mut command = Command::new(&path);
        command.env_clear().arg("--version");
        let version = bounded(command, cancel, Duration::from_secs(10))?;
        ensure!(version.status.success(), "could not identify protoc");
        Ok(Self {
            path,
            identity: CompilerIdentity {
                version: String::from_utf8(version.stdout)?.trim().into(),
                executable_digest,
            },
        })
    }

    pub fn compile(&self, schema: &str, cancel: &AtomicBool) -> Result<Vec<u8>> {
        schema_input(schema)?;
        ensure!(
            hash(&fs::read(&self.path)?) == self.identity.executable_digest,
            "compiler changed during run"
        );
        let dir = tempfile::tempdir()?;
        fs::write(dir.path().join("interface.proto"), schema)?;
        let mut command = Command::new(&self.path);
        command.env_clear().current_dir(dir.path()).args([
            "--proto_path=.",
            "--descriptor_set_out=interface.pb",
            "interface.proto",
        ]);
        let output = bounded(command, cancel, Duration::from_secs(10))?;
        if !output.status.success() {
            let diagnostics = String::from_utf8_lossy(&output.stderr);
            // Source-located parser diagnostics are semantic rejection. Termination,
            // output-file failures, and other compiler failures remain infrastructure errors.
            if output.status.code() == Some(1)
                && !diagnostics.trim().is_empty()
                && diagnostics
                    .lines()
                    .all(|line| line.starts_with("interface.proto:"))
            {
                return Err(SchemaRejection {
                    code: "INVALID_SCHEMA",
                    detail: diagnostics.into(),
                }
                .into());
            }
            bail!(
                "compiler execution failed ({}): {}",
                output.status,
                diagnostics
            );
        }
        let bytes = fs::read(dir.path().join("interface.pb"))?;
        FileDescriptorSet::decode(bytes.as_slice())
            .context("compiler emitted invalid descriptor bytes")?;
        validate_descriptor(&bytes).map_err(|error| SchemaRejection {
            code: "INVALID_SCHEMA",
            detail: error.to_string(),
        })?;
        ensure!(
            hash(&fs::read(&self.path)?) == self.identity.executable_digest,
            "compiler changed during run"
        );
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brick_contract::parse;
    const ECHO: &str = include_str!("../../../interfaces/echo.brick.toml");
    #[test]
    fn execution_failures_are_not_schema_rejections() {
        let compiler = Compiler {
            path: PathBuf::from("/definitely-missing-brick-compiler"),
            identity: CompilerIdentity {
                version: "missing".into(),
                executable_digest: "missing".into(),
            },
        };
        let schema = parse(ECHO).unwrap().schema;
        let error = compiler
            .compile(&schema, &AtomicBool::new(false))
            .unwrap_err();
        assert!(error.downcast_ref::<SchemaRejection>().is_none());
        let compiler = Compiler::discover().unwrap();
        let error = compiler
            .compile("not protobuf", &AtomicBool::new(false))
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<SchemaRejection>().unwrap().code,
            "INVALID_SCHEMA"
        );
    }

    #[cfg(unix)]
    #[test]
    fn subprocess_timeout_reaps_child() {
        let mut command = Command::new("/bin/sleep");
        command.arg("30");
        let started = Instant::now();
        let error =
            bounded(command, &AtomicBool::new(false), Duration::from_millis(30)).unwrap_err();
        assert!(error.to_string().contains("deadline"));
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}

pub mod provider;
