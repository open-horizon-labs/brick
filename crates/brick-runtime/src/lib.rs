mod codec;
pub mod generate;
pub mod scenarios;

use anyhow::{Context, Result, ensure};
use axum::{
    Router,
    http::{Request as HttpRequest, uri::PathAndQuery},
};
use brick_contract::{Bundle, Definition, message, method};
use bytes::Bytes;
use codec::RawCodec;
use prost::Message;
use prost_reflect::{DescriptorPool, DynamicMessage};
use serde::{Deserialize, Serialize};
use std::{
    convert::Infallible,
    future::Future,
    net::SocketAddr,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle};
use tonic::{
    Request, Response, Status,
    transport::{Channel, Server},
};

pub const SELF_SCHEMA: &str = include_str!("../../../interfaces/interface-check.brick.toml");
const MAX_MESSAGE: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    Shadow,
    InterfaceCheck,
    GenerateBundle,
    RunScenarios,
}

#[derive(Default)]
struct Observations {
    consumed: usize,
    violations: usize,
}

struct Runtime {
    definition: Definition,
    pool: DescriptorPool,
    mode: Mode,
    observations: Mutex<Observations>,
    compiler_slots: Arc<tokio::sync::Semaphore>,
    compiler: Option<brick_contract::Compiler>,
    cancelled: Arc<AtomicBool>,
}

impl Runtime {
    async fn invoke(&self, path: &str, payload: Bytes) -> Result<Bytes, Status> {
        let descriptor =
            method(&self.pool, path).map_err(|_| Status::unimplemented("UNKNOWN_METHOD"))?;
        let input = DynamicMessage::decode(descriptor.input(), payload)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        if input.unknown_fields().next().is_some() {
            return Err(Status::invalid_argument("UNKNOWN_FIELDS"));
        }
        match self.mode {
            Mode::Shadow => {
                let mut state = self
                    .observations
                    .lock()
                    .map_err(|_| Status::internal("state poisoned"))?;
                let Some(case) = self.definition.cases.get(state.consumed) else {
                    return Err(Status::unimplemented(
                        "SHADOW_UNSPECIFIED: no remaining declared response",
                    ));
                };
                let expected_method = method(&self.pool, &case.method).map_err(internal)?;
                let expected =
                    message(expected_method.input(), &case.request_json).map_err(internal)?;
                if descriptor != expected_method
                    || !same_message(&input, &expected).map_err(internal)?
                {
                    return Err(Status::failed_precondition(
                        "UNEXPECTED_CALL: does not match next declared case",
                    ));
                }
                let response =
                    message(descriptor.output(), &case.response_json).map_err(internal)?;
                state.consumed += 1;
                Ok(response.encode_to_vec().into())
            }
            Mode::RunScenarios => {
                let permit = self
                    .compiler_slots
                    .clone()
                    .try_acquire_owned()
                    .map_err(|_| Status::resource_exhausted("RUNNER_BUSY"))?;
                let cancel = self.cancelled.clone();
                let output = descriptor.output();
                // The supervisor owns admission through cleanup even if the RPC is dropped.
                let response = tokio::spawn(async move {
                    let _permit = permit;
                    scenarios::invoke(input, output, cancel).await
                })
                .await
                .map_err(internal)??;
                Ok(response.encode_to_vec().into())
            }
            Mode::GenerateBundle => {
                let source = input
                    .get_field_by_name("source")
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .ok_or_else(|| Status::invalid_argument("missing source"))?;
                let permit = self
                    .compiler_slots
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(internal)?;
                let compiler = self
                    .compiler
                    .clone()
                    .ok_or_else(|| Status::internal("compiler unavailable"))?;
                let cancel = self.cancelled.clone();
                let output = descriptor.output();
                let response = tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    generate::invoke(&source, &compiler, &cancel, output)
                })
                .await
                .map_err(internal)?
                .map_err(internal)?;
                if response.encoded_len() > MAX_MESSAGE {
                    return Err(Status::resource_exhausted(
                        "generated bundle exceeds 1 MiB RPC limit",
                    ));
                }
                Ok(response.encode_to_vec().into())
            }
            Mode::InterfaceCheck => {
                let schema = input
                    .get_field_by_name("schema")
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .ok_or_else(|| Status::invalid_argument("missing schema"))?;
                // Limit compiler subprocesses. Owned permit stays alive until blocking work ends.
                let permit = self
                    .compiler_slots
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(internal)?;
                let compiler = self
                    .compiler
                    .clone()
                    .ok_or_else(|| Status::internal("compiler unavailable"))?;
                let cancelled = self.cancelled.clone();
                let result = tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    match compiler.compile(&schema, &cancelled) {
                        Ok(_) => Ok((true, "")),
                        Err(error) => match error.downcast_ref::<brick_contract::SchemaRejection>()
                        {
                            Some(rejection) => Ok((false, rejection.code)),
                            None => Err(error),
                        },
                    }
                })
                .await
                .map_err(internal)?
                .map_err(internal)?;
                let json = serde_json::json!({"valid": result.0, "reason": result.1}).to_string();
                Ok(message(descriptor.output(), &json)
                    .map_err(internal)?
                    .encode_to_vec()
                    .into())
            }
        }
    }
}

fn internal(error: impl std::fmt::Display) -> Status {
    Status::internal(error.to_string())
}

fn same_message(left: &DynamicMessage, right: &DynamicMessage) -> Result<bool> {
    // Descriptor pools have separate identities; compare canonical protobuf JSON values.
    Ok(serde_json::to_value(left)? == serde_json::to_value(right)?)
}

struct Unary {
    runtime: Arc<Runtime>,
    path: String,
}
impl tonic::server::UnaryService<Bytes> for Unary {
    type Response = Bytes;
    type Future = Pin<Box<dyn Future<Output = Result<Response<Bytes>, Status>> + Send>>;
    fn call(&mut self, request: Request<Bytes>) -> Self::Future {
        let runtime = self.runtime.clone();
        let path = self.path.clone();
        Box::pin(async move {
            let result = runtime.invoke(&path, request.into_inner()).await;
            if result.is_err()
                && let Ok(mut state) = runtime.observations.lock()
            {
                state.violations += 1;
            }
            result.map(Response::new)
        })
    }
}

/// Server owns its listener and task. Dropping it signals shutdown and aborts the task.
pub struct Running {
    pub address: SocketAddr,
    runtime: Arc<Runtime>,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<(), tonic::transport::Error>>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.runtime.cancelled.store(true, Ordering::Release);
        self.runtime.compiler_slots.close();
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.task.abort();
    }
}

impl Running {
    pub fn assert_complete(&self) -> Result<()> {
        let state = self
            .runtime
            .observations
            .lock()
            .map_err(|_| anyhow::anyhow!("state poisoned"))?;
        ensure!(
            state.violations == 0,
            "{} unexpected/exhausted calls",
            state.violations
        );
        if self.runtime.mode == Mode::Shadow {
            ensure!(
                state.consumed == self.runtime.definition.cases.len(),
                "unconsumed expectations: {}",
                self.runtime.definition.cases.len() - state.consumed
            );
        }
        Ok(())
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        self.runtime.cancelled.store(true, Ordering::Release);
        self.runtime.compiler_slots.close();
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if self.runtime.mode == Mode::RunScenarios {
            tokio::time::timeout(Duration::from_secs(6), async {
                while self.runtime.compiler_slots.available_permits() == 0 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .context("owned scenario did not stop")?;
        }
        match tokio::time::timeout(Duration::from_secs(2), &mut self.task).await {
            Ok(result) => {
                result??;
            }
            Err(_) => {
                self.task.abort();
                let _ = (&mut self.task).await;
            }
        }
        tokio::time::timeout(Duration::from_secs(2), async {
            while self.runtime.compiler_slots.available_permits() == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .context("compiler did not stop during shutdown")?;
        Ok(())
    }
}

pub async fn start(bundle: &Bundle, mode: Mode, address: SocketAddr) -> Result<Running> {
    start_controlled(bundle, mode, address, Arc::new(AtomicBool::new(false))).await
}

async fn start_controlled(
    bundle: &Bundle,
    mode: Mode,
    address: SocketAddr,
    cancelled: Arc<AtomicBool>,
) -> Result<Running> {
    ensure!(!cancelled.load(Ordering::Acquire), "run cancelled");
    ensure!(
        address.ip().is_loopback(),
        "bootstrap services bind to loopback only"
    );
    let (definition, pool) = bundle.validate()?;
    if mode == Mode::InterfaceCheck {
        ensure!(
            definition.schema == brick_contract::parse(SELF_SCHEMA)?.schema,
            "real checker requires Brick's exact InterfaceCheck contract"
        );
    }
    if mode == Mode::GenerateBundle {
        ensure!(
            definition.schema == brick_contract::parse(generate::SCHEMA)?.schema,
            "real generator requires Brick's exact GenerateBundle contract"
        );
    }
    if mode == Mode::RunScenarios {
        ensure!(
            definition.schema == brick_contract::parse(scenarios::SCHEMA)?.schema,
            "real runner requires Brick's exact RunScenarios contract"
        );
    }
    let compiler = if matches!(mode, Mode::InterfaceCheck | Mode::GenerateBundle) {
        let cancel = cancelled.clone();
        Some(
            tokio::task::spawn_blocking(move || {
                brick_contract::Compiler::discover_with_cancel(&cancel)
            })
            .await??,
        )
    } else {
        None
    };
    let runtime = Arc::new(Runtime {
        definition,
        pool,
        mode,
        observations: Mutex::new(Observations::default()),
        compiler_slots: Arc::new(tokio::sync::Semaphore::new(1)),
        compiler,
        cancelled,
    });
    let listener = TcpListener::bind(address).await?;
    let address = listener.local_addr()?;
    let shared = runtime.clone();
    let router = Router::new().fallback_service(tower::service_fn(
        move |request: HttpRequest<axum::body::Body>| {
            let runtime = shared.clone();
            async move {
                let path = request.uri().path().to_owned();
                let mut grpc = tonic::server::Grpc::new(RawCodec)
                    .max_decoding_message_size(MAX_MESSAGE)
                    .max_encoding_message_size(MAX_MESSAGE);
                let response = grpc.unary(Unary { runtime, path }, request).await;
                Ok::<_, Infallible>(response)
            }
        },
    ));
    let (stop, stopped) = oneshot::channel();
    let task = tokio::spawn(async move {
        Server::builder()
            .add_routes(router.into())
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::TcpListenerStream::new(listener),
                async {
                    let _ = stopped.await;
                },
            )
            .await
    });
    Ok(Running {
        address,
        runtime,
        stop: Some(stop),
        task,
    })
}

pub async fn call(
    bundle: &Bundle,
    address: SocketAddr,
    method_name: &str,
    request_json: &str,
) -> Result<DynamicMessage> {
    call_with_timeout(
        bundle,
        address,
        method_name,
        request_json,
        Duration::from_secs(15),
    )
    .await
}

async fn call_with_timeout(
    bundle: &Bundle,
    address: SocketAddr,
    method_name: &str,
    request_json: &str,
    timeout: Duration,
) -> Result<DynamicMessage> {
    let (_, pool) = bundle.validate()?;
    let descriptor = method(&pool, method_name)?;
    let request = message(descriptor.input(), request_json)?;
    let channel = Channel::from_shared(format!("http://{address}"))?
        .connect_timeout(Duration::from_secs(2))
        .timeout(timeout)
        .connect()
        .await?;
    let mut client = tonic::client::Grpc::new(channel)
        .max_decoding_message_size(MAX_MESSAGE)
        .max_encoding_message_size(MAX_MESSAGE);
    client.ready().await?;
    let path: PathAndQuery = format!(
        "/{}/{}",
        descriptor.parent_service().full_name(),
        descriptor.name()
    )
    .parse()?;
    let mut wire_request = Request::new(Bytes::from(request.encode_to_vec()));
    wire_request.set_timeout(timeout);
    let response: Response<Bytes> = client.unary(wire_request, path, RawCodec).await?;
    Ok(DynamicMessage::decode(
        descriptor.output(),
        response.into_inner(),
    )?)
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Receipt {
    pub evidence: String,
    pub bundle_digest: String,
    pub executable_digest: String,
    pub runtime_compiler: Option<brick_contract::CompilerIdentity>,
    pub mode: Mode,
    pub passed: bool,
    pub cases_passed: usize,
    pub elapsed_ms: u128,
    pub cleanup: String,
    pub network_sandbox: String,
    pub error: Option<String>,
}

/// Executes declared cases over actual loopback gRPC. Shadow mode proves wiring only.
pub async fn verify(bundle: &Bundle, mode: Mode) -> Result<Receipt> {
    verify_controlled(bundle, mode, Arc::new(AtomicBool::new(false)), None).await
}

struct CancelOnDrop(Arc<AtomicBool>);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
async fn wait_cancel(cancel: &AtomicBool) {
    while !cancel.load(Ordering::Acquire) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn verify_controlled<'a>(
    bundle: &'a Bundle,
    mode: Mode,
    cancel: Arc<AtomicBool>,
    budget: Option<Duration>,
) -> Pin<Box<dyn Future<Output = Result<Receipt>> + Send + 'a>> {
    Box::pin(async move {
        let started = Instant::now();
        let (definition, pool) = bundle.validate()?;
        ensure!(
            !definition.cases.is_empty(),
            "verification requires at least one declared case"
        );
        let local_cancel = Arc::new(AtomicBool::new(false));
        let _cancel_on_drop = CancelOnDrop(local_cancel.clone());
        let startup = start_controlled(bundle, mode, "127.0.0.1:0".parse()?, local_cancel.clone());
        tokio::pin!(startup);
        let mut running = tokio::select! {
            result = &mut startup => result?,
            _ = wait_cancel(&cancel) => {
                local_cancel.store(true, Ordering::Release);
                let _ = startup.await;
                anyhow::bail!("run cancelled during startup");
            }
        };
        let mut count = 0;
        let execution = async {
            for case in &definition.cases {
                let descriptor = method(&pool, &case.method)?;
                let expected = message(descriptor.output(), &case.response_json)?;
                let actual = call(bundle, running.address, &case.method, &case.request_json)
                    .await
                    .with_context(|| case.name.clone())?;
                ensure!(
                    same_message(&actual, &expected)?,
                    "{}: response differs from declared expectation",
                    case.name
                );
                count += 1;
            }
            running.assert_complete()
        };
        let outcome: Result<()> = tokio::select! {
            result = execution => result,
            _ = wait_cancel(&cancel) => Err(anyhow::anyhow!("run cancelled")),
            _ = async { if let Some(budget) = budget { tokio::time::sleep(budget).await } else { std::future::pending::<()>().await } } => Err(anyhow::anyhow!("scenario deadline exceeded")),
        };
        running.shutdown().await?;
        let executable_digest = brick_contract::hash(&std::fs::read(std::env::current_exe()?)?);
        Ok(Receipt {
            evidence: if mode == Mode::Shadow {
                "shadow_wiring_verified"
            } else {
                "component_contract_verified"
            }
            .into(),
            bundle_digest: bundle.digest.clone(),
            executable_digest,
            runtime_compiler: running
                .runtime
                .compiler
                .as_ref()
                .map(|c| c.identity.clone()),
            mode,
            passed: outcome.is_ok(),
            cases_passed: count,
            elapsed_ms: started.elapsed().as_millis(),
            cleanup: "server_stopped".into(),
            network_sandbox: "not_enforced".into(),
            error: outcome.err().map(|e| format!("{e:#}")),
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    const ECHO: &str = include_str!("../../../interfaces/echo.brick.toml");

    #[tokio::test]
    async fn same_contract_runs_as_shadow_and_real_rust_checker() {
        let bundle = brick_contract::build(SELF_SCHEMA).unwrap();
        for mode in [Mode::Shadow, Mode::InterfaceCheck] {
            let receipt = verify(&bundle, mode).await.unwrap();
            assert!(receipt.passed, "{:?}", receipt.error);
            assert_eq!(receipt.cases_passed, 3);
            assert_eq!(
                receipt.runtime_compiler.is_some(),
                mode == Mode::InterfaceCheck
            );
        }
    }

    #[tokio::test]
    async fn brick_checks_its_own_interface_through_its_real_boundary() {
        let bundle = brick_contract::build(SELF_SCHEMA).unwrap();
        let mut running = start(
            &bundle,
            Mode::InterfaceCheck,
            "127.0.0.1:0".parse().unwrap(),
        )
        .await
        .unwrap();
        let own_schema = brick_contract::parse(SELF_SCHEMA).unwrap().schema;
        let request = serde_json::json!({"schema": own_schema}).to_string();
        let response = call(&bundle, running.address, "Check", &request)
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(response).unwrap(),
            serde_json::json!({"valid": true})
        );
        running.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn missing_unexpected_and_exhausted_calls_never_pass() {
        let bundle = brick_contract::build(ECHO).unwrap();
        let mut server = start(&bundle, Mode::Shadow, "127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        assert!(server.assert_complete().is_err());
        assert!(
            call(&bundle, server.address, "Say", "{\"text\":\"wrong\"}")
                .await
                .is_err()
        );
        call(&bundle, server.address, "Say", "{\"text\":\"hello\"}")
            .await
            .unwrap();
        let error = call(&bundle, server.address, "Say", "{\"text\":\"hello\"}")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("SHADOW_UNSPECIFIED"));
        assert!(server.assert_complete().is_err());
        let address = server.address;
        server.shutdown().await.unwrap();
        assert!(tokio::net::TcpStream::connect(address).await.is_err());
    }

    #[tokio::test]
    async fn real_provider_can_fail_independent_expectations_and_still_cleanup() {
        let altered = SELF_SCHEMA.replacen(
            "response_json = '{\"valid\":true}'",
            "response_json = '{\"valid\":false}'",
            1,
        );
        let bundle = brick_contract::build(&altered).unwrap();
        assert!(verify(&bundle, Mode::Shadow).await.unwrap().passed);
        let result = verify(&bundle, Mode::InterfaceCheck).await.unwrap();
        assert!(!result.passed);
        assert_eq!(result.cleanup, "server_stopped");
    }

    #[tokio::test]
    async fn dropping_a_run_cancels_listener_and_runs_have_private_state() {
        let bundle = brick_contract::build(ECHO).unwrap();
        let a = start(&bundle, Mode::Shadow, "127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        let mut b = start(&bundle, Mode::Shadow, "127.0.0.1:0".parse().unwrap())
            .await
            .unwrap();
        call(&bundle, a.address, "Say", "{\"text\":\"hello\"}")
            .await
            .unwrap();
        assert!(a.assert_complete().is_ok());
        assert!(b.assert_complete().is_err());
        let address = a.address;
        drop(a);
        // Abort is scheduled, not synchronous; allow bounded task teardown.
        tokio::time::timeout(Duration::from_secs(2), async {
            while tokio::net::TcpStream::connect(address).await.is_ok() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("dropped listener did not close");
        b.shutdown().await.unwrap();
    }
}
