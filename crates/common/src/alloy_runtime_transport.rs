//! Runtime transport that connects on first request, which can take either of an HTTP,
//! WebSocket, or IPC transport.
use alloy_json_rpc::{RequestPacket, ResponsePacket};
use alloy_pubsub::{PubSubConnect, PubSubFrontend};
use alloy_transport::{
    Authorization, BoxTransport, TransportError, TransportErrorKind, TransportFut,
};
use alloy_transport_http::Http;
use alloy_transport_ws::WsConnect;
use ethers_providers::{JwtAuth, JwtKey};
use reqwest::header::{HeaderName, HeaderValue};
use std::{str::FromStr, sync::Arc};
use thiserror::Error;
use tokio::sync::RwLock;
use tower::{Service, ServiceExt};
use url::Url;

/// An enum representing the different transports that can be used to connect to a runtime.
#[derive(Clone, Debug)]
pub enum InnerTransport {
    /// HTTP transport
    Http(Http<reqwest::Client>),
    /// WebSocket transport
    Ws(PubSubFrontend),
    // TODO: IPC
    /// IPC transport
    Ipc,
}

/// Error type for the runtime transport.
#[derive(Error, Debug)]
pub enum RuntimeTransportError {
    /// Internal transport error
    #[error(transparent)]
    TransportError(TransportError),

    /// Failed to lock the transport
    #[error("Failed to lock the transport")]
    LockError,

    /// Invalid URL scheme
    #[error("URL scheme is not supported: {0}")]
    BadScheme(String),

    /// Invalid HTTP header
    #[error("Invalid HTTP header: {0}")]
    BadHeader(String),

    /// Invalid file path
    #[error("Invalid IPC file path: {0}")]
    BadPath(String),

    /// Invalid construction of Http provider
    #[error(transparent)]
    HttpConstructionError(#[from] reqwest::Error),

    /// Invalid JWT
    #[error("Invalid JWT: {0}")]
    InvalidJwt(String),
}

/// A runtime transport that connects on first request, which can take either of an HTTP,
/// WebSocket, or IPC transport depending on the URL used.
#[derive(Clone, Debug, Error)]
pub struct RuntimeTransport {
    /// The inner actual transport used.
    inner: Arc<RwLock<Option<InnerTransport>>>,
    /// The URL to connect to.
    url: Url,
    headers: Vec<String>,
    jwt: Option<String>,
    timeout: std::time::Duration,
}

impl ::core::fmt::Display for RuntimeTransport {
    fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
        write!(f, "RuntimeTransport {}", self.url)
    }
}

impl RuntimeTransport {
    /// Create a new, unconnected transport.
    pub fn new(url: Url) -> Self {
        Self {
            inner: Arc::new(RwLock::new(None)),
            url,
            headers: vec![],
            jwt: None,
            timeout: std::time::Duration::from_secs(30),
        }
    }

    /// Add a JWT to the transport
    pub fn with_jwt(mut self, jwt: Option<String>) -> Self {
        self.jwt = jwt;
        self
    }

    /// Add headers to the transport
    pub fn with_headers(mut self, headers: Vec<String>) -> Self {
        self.headers = headers;
        self
    }

    /// Set the timeout for the transport
    pub fn with_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Connect to the runtime transport, depending on the URL scheme.
    async fn connect(&self) -> Result<InnerTransport, RuntimeTransportError> {
        match self.url.scheme() {
            "http" | "https" => {
                let mut client_builder = reqwest::Client::builder().timeout(self.timeout);

                if let Some(jwt) = self.jwt.clone() {
                    let auth = build_auth(jwt)
                        .map_err(|e| RuntimeTransportError::InvalidJwt(e.to_string()))?;

                    let mut auth_value: HeaderValue = HeaderValue::from_str(&auth.to_string())
                        .expect("Header should be valid string");
                    auth_value.set_sensitive(true);

                    let mut headers = reqwest::header::HeaderMap::new();
                    headers.insert(reqwest::header::AUTHORIZATION, auth_value);

                    for header in self.headers.iter() {
                        let make_err = || RuntimeTransportError::BadHeader(header.to_string());

                        let (key, val) = header.split_once(':').ok_or_else(make_err)?;

                        headers.insert(
                            HeaderName::from_str(key.trim()).map_err(|_| make_err())?,
                            HeaderValue::from_str(val.trim()).map_err(|_| make_err())?,
                        );
                    }

                    client_builder = client_builder.default_headers(headers);
                };

                let client = client_builder
                    .build()
                    .map_err(|e| RuntimeTransportError::HttpConstructionError(e))?;

                // todo: retry tower layer
                Ok(InnerTransport::Http(Http::with_client(client, self.url.clone())))
            }
            "ws" | "wss" => {
                let auth = self.jwt.as_ref().and_then(|jwt| build_auth(jwt.clone()).ok());
                let ws = WsConnect { url: self.url.to_string(), auth }
                    .into_service()
                    .await
                    .map_err(|e| RuntimeTransportError::TransportError(e))?;
                Ok(InnerTransport::Ws(ws))
            }
            // TODO: IPC once it's merged
            _ => Err(RuntimeTransportError::BadScheme(self.url.scheme().to_string())),
        }
    }

    /// Send a request
    pub fn request(&self, req: RequestPacket) -> TransportFut<'static> {
        let this = self.clone();
        Box::pin(async move {
            if this.inner.read().await.is_none() {
                let mut w = this.inner.write().await;
                *w = Some(this.connect().await.map_err(|e| TransportErrorKind::custom(e))?)
            }

            let mut inner = this.inner.write().await;
            let inner_mut = inner.as_mut().expect("boom");

            match inner_mut {
                InnerTransport::Http(http) => http.call(req).await,
                InnerTransport::Ws(ws) => ws.call(req).await,
                InnerTransport::Ipc => todo!(),
            }
        })
    }

    /// Convert this transport into a boxed trait object.
    pub fn boxed(self) -> BoxTransport
    where
        Self: Sized + Clone + Send + Sync + 'static,
    {
        BoxTransport::new(self)
    }
}

impl tower::Service<RequestPacket> for RuntimeTransport {
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = TransportFut<'static>;

    #[inline]
    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    #[inline]
    fn call(&mut self, req: RequestPacket) -> Self::Future {
        self.request(req)
    }
}

impl Service<RequestPacket> for &RuntimeTransport {
    type Response = ResponsePacket;
    type Error = TransportError;
    type Future = TransportFut<'static>;

    #[inline]
    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    #[inline]
    fn call(&mut self, req: RequestPacket) -> Self::Future {
        self.request(req)
    }
}

fn build_auth(jwt: String) -> eyre::Result<Authorization> {
    // Decode jwt from hex, then generate claims (iat with current timestamp)
    let jwt = hex::decode(jwt)?;
    let secret = JwtKey::from_slice(&jwt).map_err(|err| eyre::eyre!("Invalid JWT: {}", err))?;
    let auth = JwtAuth::new(secret, None, None);
    let token = auth.generate_token()?;

    // Essentially unrolled ethers-rs new_with_auth to accomodate the custom timeout
    let auth = Authorization::Bearer(token);

    Ok(auth)
}
