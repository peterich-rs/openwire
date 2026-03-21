use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use http::{Request, Response, StatusCode};
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use openwire_core::{
    BoxFuture, CallContext, ConnectionId, DnsResolver, EventListener, EventListenerFactory,
    RequestBody, ResponseBody, SharedEventListener, WireError,
};
use rcgen::generate_simple_self_signed;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::net::TcpListener;
use tokio::sync::oneshot;

pub type TestResponse = Response<Full<Bytes>>;

pub struct TestServer {
    addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    tls_root_pem: Option<String>,
}

impl TestServer {
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn http_url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }

    pub fn https_url(&self, path: &str) -> String {
        format!("https://{}{}", self.addr, path)
    }

    pub fn tls_root_pem(&self) -> Option<&str> {
        self.tls_root_pem.as_deref()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
    }
}

pub async fn spawn_http1<F, Fut>(handler: F) -> TestServer
where
    F: Fn(Request<Incoming>) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = TestResponse> + Send + 'static,
{
    spawn_server(handler, None).await
}

pub async fn spawn_https_http1<F, Fut>(handler: F) -> TestServer
where
    F: Fn(Request<Incoming>) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = TestResponse> + Send + 'static,
{
    let cert = generate_simple_self_signed(vec!["localhost".to_owned()])
        .expect("failed to create self-signed certificate");
    let cert_der: CertificateDer<'static> = CertificateDer::from(cert.cert.der().to_vec());
    let key_der = PrivateKeyDer::from(PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()));
    let mut config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .expect("failed to create TLS server config");
    config.alpn_protocols = vec![b"http/1.1".to_vec()];

    spawn_server(
        handler,
        Some((
            tokio_rustls::TlsAcceptor::from(Arc::new(config)),
            cert.cert.pem(),
        )),
    )
    .await
}

pub fn text_response(status: StatusCode, body: impl Into<Bytes>) -> TestResponse {
    Response::builder()
        .status(status)
        .body(Full::new(body.into()))
        .expect("response build")
}

pub fn ok_text(body: impl Into<Bytes>) -> TestResponse {
    text_response(StatusCode::OK, body)
}

pub async fn collect_request_body(request: Request<Incoming>) -> Bytes {
    http_body_util::BodyExt::collect(request.into_body())
        .await
        .expect("collect request body")
        .to_bytes()
}

#[derive(Clone, Default)]
pub struct RecordingEventListenerFactory {
    events: Arc<Mutex<Vec<String>>>,
}

impl RecordingEventListenerFactory {
    pub fn events(&self) -> Vec<String> {
        self.events.lock().expect("event lock").clone()
    }
}

impl EventListenerFactory for RecordingEventListenerFactory {
    fn create(&self, _request: &Request<RequestBody>) -> SharedEventListener {
        Arc::new(RecordingEventListener {
            events: self.events.clone(),
        })
    }
}

struct RecordingEventListener {
    events: Arc<Mutex<Vec<String>>>,
}

impl RecordingEventListener {
    fn push(&self, value: impl Into<String>) {
        self.events.lock().expect("event lock").push(value.into());
    }
}

impl EventListener for RecordingEventListener {
    fn call_start(&self, _ctx: &CallContext, request: &Request<RequestBody>) {
        self.push(format!("call_start {} {}", request.method(), request.uri()));
    }

    fn call_end(&self, _ctx: &CallContext, response: &Response<ResponseBody>) {
        self.push(format!("call_end {}", response.status()));
    }

    fn call_failed(&self, _ctx: &CallContext, error: &WireError) {
        self.push(format!("call_failed {:?}", error.kind()));
    }

    fn dns_start(&self, _ctx: &CallContext, host: &str, _port: u16) {
        self.push(format!("dns_start {host}"));
    }

    fn dns_end(&self, _ctx: &CallContext, host: &str, addrs: &[SocketAddr]) {
        self.push(format!("dns_end {host} {}", addrs.len()));
    }

    fn connect_failed(&self, _ctx: &CallContext, addr: SocketAddr, error: &WireError) {
        self.push(format!("connect_failed {addr} {}", error.kind()));
    }

    fn connect_end(&self, _ctx: &CallContext, connection_id: ConnectionId, addr: SocketAddr) {
        self.push(format!("connect_end {} {}", connection_id.as_u64(), addr));
    }

    fn request_body_end(&self, _ctx: &CallContext, bytes_sent: u64) {
        self.push(format!("request_body_end {bytes_sent}"));
    }

    fn response_headers_start(&self, _ctx: &CallContext) {
        self.push("response_headers_start");
    }

    fn response_headers_end(&self, _ctx: &CallContext, response: &Response<ResponseBody>) {
        self.push(format!("response_headers_end {}", response.status()));
    }

    fn response_body_failed(&self, _ctx: &CallContext, error: &WireError) {
        self.push(format!("response_body_failed {}", error.kind()));
    }

    fn response_body_end(&self, _ctx: &CallContext, bytes_read: u64) {
        self.push(format!("response_body_end {bytes_read}"));
    }

    fn connection_acquired(&self, _ctx: &CallContext, connection_id: ConnectionId, reused: bool) {
        self.push(format!(
            "connection_acquired {} reused={reused}",
            connection_id.as_u64()
        ));
    }

    fn connection_released(&self, _ctx: &CallContext, connection_id: ConnectionId) {
        self.push(format!("connection_released {}", connection_id.as_u64()));
    }

    fn retry(&self, _ctx: &CallContext, attempt: u32, reason: &str) {
        self.push(format!("retry {attempt} {reason}"));
    }

    fn redirect(&self, _ctx: &CallContext, attempt: u32, location: &http::Uri) {
        self.push(format!("redirect {attempt} {location}"));
    }
}

#[derive(Clone)]
pub struct StaticDnsResolver {
    addr: SocketAddr,
}

impl StaticDnsResolver {
    pub fn new(addr: SocketAddr) -> Self {
        Self { addr }
    }
}

impl DnsResolver for StaticDnsResolver {
    fn resolve(
        &self,
        ctx: CallContext,
        host: String,
        port: u16,
    ) -> BoxFuture<Result<Vec<SocketAddr>, WireError>> {
        let mut addr = self.addr;
        addr.set_port(port);
        Box::pin(async move {
            ctx.listener().dns_start(&ctx, &host, port);
            ctx.listener().dns_end(&ctx, &host, &[addr]);
            Ok(vec![addr])
        })
    }
}

async fn spawn_server<F, Fut>(
    handler: F,
    tls: Option<(tokio_rustls::TlsAcceptor, String)>,
) -> TestServer
where
    F: Fn(Request<Incoming>) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = TestResponse> + Send + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test listener");
    let addr = listener.local_addr().expect("local addr");
    let (shutdown_tx, mut shutdown_rx) = oneshot::channel();
    let tls_root_pem = tls.as_ref().map(|(_, pem)| pem.clone());

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => {
                    let Ok((stream, _peer)) = accepted else {
                        break;
                    };
                    let handler = handler.clone();
                    let tls = tls.clone();
                    tokio::spawn(async move {
                        let service = service_fn(move |request| {
                            let handler = handler.clone();
                            async move { Ok::<_, Infallible>(handler(request).await) }
                        });

                        match tls {
                            Some((acceptor, _)) => {
                                let Ok(stream) = acceptor.accept(stream).await else {
                                    return;
                                };
                                let _ = http1::Builder::new()
                                    .serve_connection(TokioIo::new(stream), service)
                                    .await;
                            }
                            None => {
                                let _ = http1::Builder::new()
                                    .serve_connection(TokioIo::new(stream), service)
                                    .await;
                            }
                        }
                    });
                }
            }
        }
    });

    TestServer {
        addr,
        shutdown: Some(shutdown_tx),
        tls_root_pem,
    }
}
