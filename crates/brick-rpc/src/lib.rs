//! Small shared transport and finite shadow host. No built-in business providers.
mod codec;
use anyhow::{Result, ensure};
use axum::{
    Router,
    http::{Request as HttpRequest, uri::PathAndQuery},
};
use brick_contract::{Bundle, Definition, contract_id, message, method};
use bytes::Bytes;
pub use codec::RawCodec;
use prost::Message;
use prost_reflect::{DescriptorPool, DynamicMessage, MethodDescriptor};
use std::{
    convert::Infallible,
    future::Future,
    net::SocketAddr,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::{Semaphore, oneshot},
    task::JoinHandle,
};
use tonic::{
    Request, Response, Status,
    transport::{Channel, Server},
};
const LIMIT: usize = 1024 * 1024;
const REVISION: &str = "brick-contract-id";
pub type Reply = Pin<Box<dyn Future<Output = Result<DynamicMessage, Status>> + Send>>;
pub trait Handler: Send + Sync + 'static {
    fn call(
        &self,
        method: MethodDescriptor,
        input: DynamicMessage,
        cancel: Arc<AtomicBool>,
    ) -> Reply;
    fn complete(&self) -> Result<()> {
        Ok(())
    }
}
pub fn internal(error: impl std::fmt::Display) -> Status {
    Status::internal(error.to_string())
}
struct State {
    pool: DescriptorPool,
    id: String,
    handler: Arc<dyn Handler>,
    cancel: Arc<AtomicBool>,
    slots: Arc<Semaphore>,
    violations: AtomicUsize,
}
struct Unary {
    state: Arc<State>,
    path: String,
}
impl tonic::server::UnaryService<Bytes> for Unary {
    type Response = Bytes;
    type Future = Pin<Box<dyn Future<Output = Result<Response<Bytes>, Status>> + Send>>;
    fn call(&mut self, request: Request<Bytes>) -> Self::Future {
        let state = self.state.clone();
        let path = self.path.clone();
        Box::pin(async move {
            let result = async {
                if request
                    .metadata()
                    .get(REVISION)
                    .and_then(|v| v.to_str().ok())
                    != Some(state.id.as_str())
                {
                    return Err(Status::failed_precondition("CONTRACT_MISMATCH"));
                }
                let method = method(&state.pool, &path)
                    .map_err(|_| Status::unimplemented("UNKNOWN_METHOD"))?;
                let input = DynamicMessage::decode(method.input(), request.into_inner())
                    .map_err(|e| Status::invalid_argument(e.to_string()))?;
                if input.unknown_fields().next().is_some() {
                    return Err(Status::invalid_argument("UNKNOWN_FIELDS"));
                }
                let permit = state
                    .slots
                    .clone()
                    .try_acquire_owned()
                    .map_err(|_| Status::resource_exhausted("PROVIDER_BUSY"))?;
                let handler = state.handler.clone();
                let cancel = state.cancel.clone();
                // Admission belongs to work, not to the client's response future.
                let output = tokio::spawn(async move {
                    let _permit = permit;
                    handler.call(method, input, cancel).await
                })
                .await
                .map_err(internal)??;
                if output.encoded_len() > LIMIT {
                    return Err(Status::resource_exhausted("RESPONSE_TOO_LARGE"));
                }
                let mut response = Response::new(Bytes::from(output.encode_to_vec()));
                response
                    .metadata_mut()
                    .insert(REVISION, state.id.parse().map_err(internal)?);
                Ok(response)
            }
            .await;
            if result.is_err() {
                state.violations.fetch_add(1, Ordering::Relaxed);
            }
            result
        })
    }
}
pub struct Running {
    pub address: SocketAddr,
    state: Arc<State>,
    stop: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<(), tonic::transport::Error>>,
}
impl Drop for Running {
    fn drop(&mut self) {
        self.state.cancel.store(true, Ordering::Release);
        self.state.slots.close();
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.task.abort();
    }
}
impl Running {
    pub fn assert_complete(&self) -> Result<()> {
        ensure!(
            self.state.violations.load(Ordering::Relaxed) == 0,
            "host rejected unexpected calls"
        );
        self.state.handler.complete()
    }
    pub async fn shutdown(&mut self) -> Result<()> {
        self.state.cancel.store(true, Ordering::Release);
        self.state.slots.close();
        // Compiler work polls cancellation; RPC dependency waits have a 15s bound.
        tokio::time::timeout(Duration::from_secs(17), async {
            while self.state.slots.available_permits() == 0 {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        match tokio::time::timeout(Duration::from_secs(2), &mut self.task).await {
            Ok(r) => r??,
            Err(_) => {
                self.task.abort();
                let _ = (&mut self.task).await;
            }
        }
        Ok(())
    }
}
pub async fn start(
    bundle: &Bundle,
    handler: Arc<dyn Handler>,
    address: SocketAddr,
) -> Result<Running> {
    ensure!(address.ip().is_loopback(), "loopback only");
    let (_, pool) = bundle.validate()?;
    let state = Arc::new(State {
        pool,
        id: contract_id(&bundle.source)?,
        handler,
        cancel: Arc::new(AtomicBool::new(false)),
        slots: Arc::new(Semaphore::new(1)),
        violations: AtomicUsize::new(0),
    });
    let listener = TcpListener::bind(address).await?;
    let address = listener.local_addr()?;
    let shared = state.clone();
    let router = Router::new().fallback_service(tower::service_fn(
        move |request: HttpRequest<axum::body::Body>| {
            let state = shared.clone();
            async move {
                let path = request.uri().path().to_owned();
                let response = tonic::server::Grpc::new(RawCodec)
                    .max_decoding_message_size(LIMIT)
                    .max_encoding_message_size(LIMIT)
                    .unary(Unary { state, path }, request)
                    .await;
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
        state,
        stop: Some(stop),
        task,
    })
}
pub async fn call(
    bundle: &Bundle,
    address: SocketAddr,
    name: &str,
    input: &DynamicMessage,
) -> Result<DynamicMessage> {
    ensure!(address.ip().is_loopback(), "loopback only");
    let (_, pool) = bundle.validate()?;
    let method = method(&pool, name)?;
    let id = contract_id(&bundle.source)?;
    let channel = Channel::from_shared(format!("http://{address}"))?
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(15))
        .connect()
        .await?;
    let mut client = tonic::client::Grpc::new(channel)
        .max_decoding_message_size(LIMIT)
        .max_encoding_message_size(LIMIT);
    client.ready().await?;
    let path: PathAndQuery =
        format!("/{}/{}", method.parent_service().full_name(), method.name()).parse()?;
    let mut request = Request::new(Bytes::from(input.encode_to_vec()));
    request.set_timeout(Duration::from_secs(15));
    request.metadata_mut().insert(REVISION, id.parse()?);
    let response: Response<Bytes> = client.unary(request, path, RawCodec).await?;
    ensure!(
        response
            .metadata()
            .get(REVISION)
            .and_then(|v| v.to_str().ok())
            == Some(id.as_str()),
        "peer contract mismatch"
    );
    Ok(DynamicMessage::decode(
        method.output(),
        response.into_inner(),
    )?)
}
pub struct Shadow {
    definition: Definition,
    pool: DescriptorPool,
    state: Mutex<(usize, usize)>,
}
impl Shadow {
    pub fn new(bundle: &Bundle) -> Result<Self> {
        let (definition, pool) = bundle.validate()?;
        Ok(Self {
            definition,
            pool,
            state: Mutex::new((0, 0)),
        })
    }
}
impl Handler for Shadow {
    fn call(
        &self,
        method: MethodDescriptor,
        input: DynamicMessage,
        _cancel: Arc<AtomicBool>,
    ) -> Reply {
        let result = (|| {
            let mut state = self.state.lock().map_err(internal)?;
            let result = (|| {
                let case = self
                    .definition
                    .cases
                    .get(state.0)
                    .ok_or_else(|| Status::unimplemented("SHADOW_UNSPECIFIED"))?;
                let expected =
                    brick_contract::method(&self.pool, &case.method).map_err(internal)?;
                if method.full_name() != expected.full_name()
                    || serde_json::to_value(&input).map_err(internal)?
                        != serde_json::to_value(
                            message(expected.input(), &case.request_json).map_err(internal)?,
                        )
                        .map_err(internal)?
                {
                    return Err(Status::failed_precondition("UNEXPECTED_CALL"));
                }
                message(method.output(), &case.response_json).map_err(internal)
            })();
            if result.is_ok() {
                state.0 += 1;
            } else {
                state.1 += 1;
            }
            result
        })();
        Box::pin(async move { result })
    }
    fn complete(&self) -> Result<()> {
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("poisoned shadow"))?;
        ensure!(
            state.1 == 0 && state.0 == self.definition.cases.len(),
            "shadow expectations incomplete or violated"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn host_rejection_cannot_be_hidden_by_consuming_valid_cases() {
        let bundle: Bundle =
            serde_json::from_str(include_str!("../../../fixtures/echo.bundle.json")).unwrap();
        let mut server = start(
            &bundle,
            Arc::new(Shadow::new(&bundle).unwrap()),
            "127.0.0.1:0".parse().unwrap(),
        )
        .await
        .unwrap();
        let channel = Channel::from_shared(format!("http://{}", server.address))
            .unwrap()
            .connect()
            .await
            .unwrap();
        let mut client = tonic::client::Grpc::new(channel);
        client.ready().await.unwrap();
        let mut request = Request::new(Bytes::new());
        request.metadata_mut().insert(
            REVISION,
            contract_id(&bundle.source).unwrap().parse().unwrap(),
        );
        let response: Result<Response<Bytes>, _> = client
            .unary(
                request,
                PathAndQuery::from_static("/example.echo.v1.Echo/Unknown"),
                RawCodec,
            )
            .await;
        assert!(response.is_err());
        let (definition, pool) = bundle.validate().unwrap();
        let case = &definition.cases[0];
        let req = message(method(&pool, "Say").unwrap().input(), &case.request_json).unwrap();
        call(&bundle, server.address, "Say", &req).await.unwrap();
        assert!(server.assert_complete().is_err());
        server.shutdown().await.unwrap();
    }
}
