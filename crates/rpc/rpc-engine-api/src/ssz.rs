//! SSZ transport helpers for the authenticated Engine API server.

use crate::EngineApi;
use alloy_primitives::{B256, B64};
use alloy_rpc_types_engine::{
    ExecutionPayloadV3, ForkchoiceState, ForkchoiceUpdated, PayloadStatus, PayloadStatusEnum,
};
use async_trait::async_trait;
use http::{header::CONTENT_TYPE, Method, Response, StatusCode};
use http_body_util::BodyExt;
use jsonrpsee::{
    core::RpcResult,
    server::{HttpBody, HttpRequest, HttpResponse},
};
use reth_chainspec::EthereumHardforks;
use reth_engine_primitives::{EngineApiValidator, EngineTypes};
use reth_rpc_api::EngineApiServer;
use reth_storage_api::{BlockReader, HeaderProvider, StateProviderFactory};
use reth_transaction_pool::TransactionPool;
use ssz::{Decode, Encode};
use ssz_derive::{Decode, Encode};
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, RwLock},
    task::{Context, Poll},
};
use tower::{BoxError, Layer, Service};

/// Exact path for the SSZ `engine_newPayloadV3` route.
pub const ENGINE_NEW_PAYLOAD_V3_SSZ_ROUTE: &str = "/engine/newPayloadV3";

/// Exact path for the SSZ `engine_forkchoiceUpdatedV3` route.
pub const ENGINE_FORKCHOICE_UPDATED_V3_SSZ_ROUTE: &str = "/engine/forkchoiceUpdatedV3";

/// Shared handle used to publish the constructed Engine API instance to transport middleware.
#[derive(Debug)]
pub struct SharedEngineApi<Engine> {
    inner: Arc<RwLock<Option<Engine>>>,
}

impl<Engine> SharedEngineApi<Engine> {
    /// Stores the Engine API instance.
    pub fn set(&self, engine: Engine) {
        *self.inner.write().expect("engine api lock poisoned") = Some(engine);
    }
}

impl<Engine: Clone> SharedEngineApi<Engine> {
    fn get(&self) -> Option<Engine> {
        self.inner.read().expect("engine api lock poisoned").clone()
    }
}

impl<Engine> Clone for SharedEngineApi<Engine> {
    fn clone(&self) -> Self {
        Self { inner: Arc::clone(&self.inner) }
    }
}

impl<Engine> Default for SharedEngineApi<Engine> {
    fn default() -> Self {
        Self { inner: Arc::new(RwLock::new(None)) }
    }
}

/// SSZ request body for `engine_newPayloadV3`.
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct SszNewPayloadV3Request {
    /// The execution payload body.
    pub execution_payload: ExecutionPayloadV3,
    /// The blob versioned hashes associated with the payload.
    pub versioned_hashes: Vec<B256>,
    /// The parent beacon block root required by Cancun+ payload validation.
    pub parent_beacon_block_root: B256,
}

impl SszNewPayloadV3Request {
    /// Returns the SSZ-encoded request bytes.
    pub fn to_ssz_bytes(&self) -> Vec<u8> {
        self.as_ssz_bytes()
    }

    /// Decodes the request from SSZ bytes.
    pub fn from_ssz_bytes(bytes: &[u8]) -> Result<Self, ssz::DecodeError> {
        <Self as Decode>::from_ssz_bytes(bytes)
    }
}

/// SSZ forkchoice state wrapper.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub struct SszForkchoiceState {
    /// Hash of the head block.
    pub head_block_hash: B256,
    /// Hash of the safe block.
    pub safe_block_hash: B256,
    /// Hash of the finalized block.
    pub finalized_block_hash: B256,
}

impl From<ForkchoiceState> for SszForkchoiceState {
    fn from(value: ForkchoiceState) -> Self {
        Self {
            head_block_hash: value.head_block_hash,
            safe_block_hash: value.safe_block_hash,
            finalized_block_hash: value.finalized_block_hash,
        }
    }
}

impl From<SszForkchoiceState> for ForkchoiceState {
    fn from(value: SszForkchoiceState) -> Self {
        Self {
            head_block_hash: value.head_block_hash,
            safe_block_hash: value.safe_block_hash,
            finalized_block_hash: value.finalized_block_hash,
        }
    }
}

/// SSZ request body for `engine_forkchoiceUpdatedV3`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Encode, Decode)]
pub struct SszForkchoiceUpdatedV3Request {
    /// The forkchoice state to apply.
    pub forkchoice_state: SszForkchoiceState,
}

impl SszForkchoiceUpdatedV3Request {
    /// Returns the SSZ-encoded request bytes.
    pub fn to_ssz_bytes(&self) -> Vec<u8> {
        self.as_ssz_bytes()
    }

    /// Decodes the request from SSZ bytes.
    pub fn from_ssz_bytes(bytes: &[u8]) -> Result<Self, ssz::DecodeError> {
        <Self as Decode>::from_ssz_bytes(bytes)
    }
}

/// Compact SSZ representation of [`PayloadStatus`].
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct SszPayloadStatus {
    /// Numeric payload status code: 0=VALID, 1=INVALID, 2=SYNCING, 3=ACCEPTED.
    pub status: u8,
    /// The latest valid hash when one is available.
    pub latest_valid_hash: B256,
    /// Whether `latest_valid_hash` is set.
    pub has_latest_valid_hash: bool,
    /// Validation error bytes for INVALID payloads.
    pub validation_error: Vec<u8>,
}

impl SszPayloadStatus {
    /// Returns the SSZ-encoded response bytes.
    pub fn to_ssz_bytes(&self) -> Vec<u8> {
        self.as_ssz_bytes()
    }

    /// Decodes the response from SSZ bytes.
    pub fn from_ssz_bytes(bytes: &[u8]) -> Result<Self, ssz::DecodeError> {
        <Self as Decode>::from_ssz_bytes(bytes)
    }
}

impl From<PayloadStatus> for SszPayloadStatus {
    fn from(value: PayloadStatus) -> Self {
        let (status, validation_error) = match value.status {
            PayloadStatusEnum::Valid => (0, Vec::new()),
            PayloadStatusEnum::Invalid { validation_error } => (1, validation_error.into_bytes()),
            PayloadStatusEnum::Syncing => (2, Vec::new()),
            PayloadStatusEnum::Accepted => (3, Vec::new()),
        };

        Self {
            status,
            latest_valid_hash: value.latest_valid_hash.unwrap_or_default(),
            has_latest_valid_hash: value.latest_valid_hash.is_some(),
            validation_error,
        }
    }
}

/// Compact SSZ representation of [`ForkchoiceUpdated`].
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct SszForkchoiceUpdated {
    /// Payload validation result.
    pub payload_status: SszPayloadStatus,
    /// Payload id returned by the EL when a build job is started.
    pub payload_id: B64,
    /// Whether a payload id is present.
    pub has_payload_id: bool,
}

impl SszForkchoiceUpdated {
    /// Returns the SSZ-encoded response bytes.
    pub fn to_ssz_bytes(&self) -> Vec<u8> {
        self.as_ssz_bytes()
    }

    /// Decodes the response from SSZ bytes.
    pub fn from_ssz_bytes(bytes: &[u8]) -> Result<Self, ssz::DecodeError> {
        <Self as Decode>::from_ssz_bytes(bytes)
    }
}

impl From<ForkchoiceUpdated> for SszForkchoiceUpdated {
    fn from(value: ForkchoiceUpdated) -> Self {
        Self {
            payload_status: value.payload_status.into(),
            payload_id: value.payload_id.map(|id| id.0).unwrap_or_default(),
            has_payload_id: value.payload_id.is_some(),
        }
    }
}

/// Minimal async interface needed by the SSZ transport proxy.
#[async_trait]
pub trait SszEngineApi: Clone + Send + Sync + 'static {
    /// Handles `engine_newPayloadV3`.
    async fn new_payload_v3_ssz(
        &self,
        payload: ExecutionPayloadV3,
        versioned_hashes: Vec<B256>,
        parent_beacon_block_root: B256,
    ) -> RpcResult<PayloadStatus>;

    /// Handles `engine_forkchoiceUpdatedV3` without payload attributes.
    async fn forkchoice_updated_v3_ssz(
        &self,
        forkchoice_state: ForkchoiceState,
    ) -> RpcResult<ForkchoiceUpdated>;
}

#[async_trait]
impl<Provider, EngineT, Pool, Validator, ChainSpec> SszEngineApi
    for EngineApi<Provider, EngineT, Pool, Validator, ChainSpec>
where
    Provider: HeaderProvider + BlockReader + StateProviderFactory + 'static,
    EngineT: EngineTypes<ExecutionData = alloy_rpc_types_engine::ExecutionData>,
    Pool: TransactionPool + 'static,
    Validator: EngineApiValidator<EngineT>,
    ChainSpec: EthereumHardforks + Send + Sync + 'static,
{
    async fn new_payload_v3_ssz(
        &self,
        payload: ExecutionPayloadV3,
        versioned_hashes: Vec<B256>,
        parent_beacon_block_root: B256,
    ) -> RpcResult<PayloadStatus> {
        EngineApiServer::<EngineT>::new_payload_v3(
            self,
            payload,
            versioned_hashes,
            parent_beacon_block_root,
        )
        .await
    }

    async fn forkchoice_updated_v3_ssz(
        &self,
        forkchoice_state: ForkchoiceState,
    ) -> RpcResult<ForkchoiceUpdated> {
        EngineApiServer::<EngineT>::fork_choice_updated_v3(self, forkchoice_state, None).await
    }
}

/// HTTP middleware layer that serves a small SSZ Engine API surface on exact routes.
#[derive(Clone, Debug)]
pub struct SszEngineApiLayer<Engine> {
    engine: SharedEngineApi<Engine>,
}

impl<Engine> SszEngineApiLayer<Engine> {
    /// Creates a new layer using the shared Engine API handle.
    pub fn new(engine: SharedEngineApi<Engine>) -> Self {
        Self { engine }
    }
}

impl<S, Engine> Layer<S> for SszEngineApiLayer<Engine>
where
    Engine: SszEngineApi,
{
    type Service = SszEngineApiService<S, Engine>;

    fn layer(&self, inner: S) -> Self::Service {
        SszEngineApiService { inner, engine: self.engine.clone() }
    }
}

/// [`Service`] that intercepts a couple of exact SSZ Engine API routes.
#[derive(Clone, Debug)]
pub struct SszEngineApiService<S, Engine> {
    inner: S,
    engine: SharedEngineApi<Engine>,
}

impl<S, Engine> Service<HttpRequest> for SszEngineApiService<S, Engine>
where
    S: Service<HttpRequest, Response = HttpResponse, Error = BoxError> + Send + Clone,
    S::Future: Send + 'static,
    Engine: SszEngineApi,
{
    type Response = HttpResponse;
    type Error = BoxError;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, request: HttpRequest) -> Self::Future {
        let path = request.uri().path().to_owned();

        if path == ENGINE_NEW_PAYLOAD_V3_SSZ_ROUTE || path == ENGINE_FORKCHOICE_UPDATED_V3_SSZ_ROUTE
        {
            let engine = self.engine.get();
            Box::pin(async move { handle_ssz_route(request, path, engine).await })
        } else {
            let fut = self.inner.call(request);
            Box::pin(fut)
        }
    }
}

async fn handle_ssz_route<Engine>(
    request: HttpRequest,
    path: String,
    engine: Option<Engine>,
) -> Result<HttpResponse, BoxError>
where
    Engine: SszEngineApi,
{
    if request.method() != Method::POST {
        return Ok(text_response(StatusCode::METHOD_NOT_ALLOWED, "method not allowed"));
    }

    let Some(content_type) =
        request.headers().get(CONTENT_TYPE).and_then(|value| value.to_str().ok())
    else {
        return Ok(text_response(StatusCode::UNSUPPORTED_MEDIA_TYPE, "missing content type"));
    };

    if !content_type.starts_with("application/ssz") {
        return Ok(text_response(StatusCode::UNSUPPORTED_MEDIA_TYPE, "expected application/ssz"));
    }

    let Some(engine) = engine else {
        return Ok(text_response(StatusCode::SERVICE_UNAVAILABLE, "engine api not ready"));
    };

    let body = request.into_body().collect().await?.to_bytes();

    if path == ENGINE_NEW_PAYLOAD_V3_SSZ_ROUTE {
        let request = match SszNewPayloadV3Request::from_ssz_bytes(&body) {
            Ok(request) => request,
            Err(err) => {
                return Ok(text_response(
                    StatusCode::BAD_REQUEST,
                    &format!("invalid ssz payload: {err:?}"),
                ))
            }
        };

        let status = match engine
            .new_payload_v3_ssz(
                request.execution_payload,
                request.versioned_hashes,
                request.parent_beacon_block_root,
            )
            .await
        {
            Ok(status) => status,
            Err(err) => {
                return Ok(text_response(
                    StatusCode::BAD_REQUEST,
                    &format!("engine_newPayloadV3 failed: {err}"),
                ))
            }
        };

        return Ok(ssz_response(SszPayloadStatus::from(status).to_ssz_bytes()))
    }

    let request = match SszForkchoiceUpdatedV3Request::from_ssz_bytes(&body) {
        Ok(request) => request,
        Err(err) => {
            return Ok(text_response(
                StatusCode::BAD_REQUEST,
                &format!("invalid ssz forkchoice request: {err:?}"),
            ))
        }
    };

    let updated = match engine.forkchoice_updated_v3_ssz(request.forkchoice_state.into()).await {
        Ok(updated) => updated,
        Err(err) => {
            return Ok(text_response(
                StatusCode::BAD_REQUEST,
                &format!("engine_forkchoiceUpdatedV3 failed: {err}"),
            ))
        }
    };

    Ok(ssz_response(SszForkchoiceUpdated::from(updated).to_ssz_bytes()))
}

fn ssz_response(body: Vec<u8>) -> HttpResponse {
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, "application/ssz")
        .body(HttpBody::from(body))
        .expect("valid ssz response")
}

fn text_response(status: StatusCode, body: &str) -> HttpResponse {
    Response::builder()
        .status(status)
        .header(CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(HttpBody::from(body.to_owned()))
        .expect("valid text response")
}
