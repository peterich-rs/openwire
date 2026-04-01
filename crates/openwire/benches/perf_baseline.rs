use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use futures_util::future::try_join_all;
use http::Request;
use openwire::{
    BoxFuture, CallContext, Client, DnsResolver, RequestBody, RustlsTlsConnector, TcpConnector,
    WireError,
};
use openwire_core::BoxConnection;
use openwire_test::{ok_text, spawn_http1, spawn_https_http2, TestServer};
use openwire_tokio::TokioTcpConnector;
use tokio::runtime::Runtime;

fn perf_baseline(c: &mut Criterion) {
    let runtime = Runtime::new().expect("runtime");

    let http1_server = runtime.block_on(spawn_http1(|_request| async move { ok_text("bench") }));
    let h2_server = runtime.block_on(spawn_https_http2(
        |_request| async move { ok_text("bench") },
    ));

    let http1_client = Client::builder().build().expect("http1 client");
    let h2_client = Client::builder()
        .max_requests_per_host(usize::MAX)
        .tls_connector(tls_connector(&h2_server))
        .build()
        .expect("h2 client");
    let race_resolver = MultiAddrResolver::new([(
        "openwire.test".to_owned(),
        vec![
            SocketAddr::from(([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 1], 9001)),
            SocketAddr::from(([192, 0, 2, 20], 9002)),
        ],
    )]);
    let race_connector = ScriptedRaceTcpConnector::new([
        (
            SocketAddr::from(([0x20, 0x01, 0x0d, 0xb8, 0, 0, 0, 1], 9001)),
            TcpAttemptScript {
                actual_addr: http1_server.addr(),
                delay: Duration::from_millis(600),
            },
        ),
        (
            SocketAddr::from(([192, 0, 2, 20], 9002)),
            TcpAttemptScript {
                actual_addr: http1_server.addr(),
                delay: Duration::from_millis(10),
            },
        ),
    ]);
    let race_client = Client::builder()
        .dns_resolver(race_resolver)
        .tcp_connector(race_connector)
        .pool_idle_timeout(Duration::ZERO)
        .build()
        .expect("race client");

    let http1_url = http1_server.http_url("/bench-http1");
    let h2_url = format!("https://localhost:{}/bench-h2", h2_server.addr().port());
    let race_url = format!(
        "http://openwire.test:{}/bench-cold-race",
        http1_server.addr().port()
    );

    runtime.block_on(async {
        single_get(&http1_client, &http1_url).await;
        single_get(&h2_client, &h2_url).await;
        single_get(&race_client, &race_url).await;
    });

    let mut group = c.benchmark_group("openwire.local");

    group.throughput(Throughput::Elements(1));
    group.bench_function("http1_warm_pooled_get", |b| {
        b.to_async(&runtime)
            .iter(|| async { single_get(&http1_client, &http1_url).await });
    });

    group.throughput(Throughput::Elements(1));
    group.bench_function("https_h2_warm_pooled_get", |b| {
        b.to_async(&runtime)
            .iter(|| async { single_get(&h2_client, &h2_url).await });
    });

    group.throughput(Throughput::Elements(8));
    group.bench_function("https_h2_8_concurrent_gets", |b| {
        b.to_async(&runtime)
            .iter(|| async { concurrent_gets(&h2_client, &h2_url, 8).await });
    });

    group.throughput(Throughput::Elements(1));
    group.bench_function("http1_cold_fast_fallback_get", |b| {
        b.to_async(&runtime)
            .iter(|| async { single_get(&race_client, &race_url).await });
    });

    group.finish();
}

async fn single_get(client: &Client, uri: &str) {
    let response = client
        .execute(empty_request(uri))
        .await
        .expect("single get response");
    let _ = response.into_body().bytes().await.expect("single get body");
}

async fn concurrent_gets(client: &Client, uri: &str, count: usize) {
    let requests = (0..count).map(|_| {
        let client = client.clone();
        async move {
            let response = client
                .execute(empty_request(uri))
                .await
                .expect("concurrent response");
            let _ = response.into_body().bytes().await.expect("concurrent body");
        }
    });

    try_join_all(requests.map(|future| async move {
        future.await;
        Ok::<(), ()>(())
    }))
    .await
    .expect("concurrent requests");
}

fn empty_request(uri: &str) -> Request<RequestBody> {
    Request::builder()
        .uri(uri)
        .body(RequestBody::empty())
        .expect("request")
}

fn tls_connector(server: &TestServer) -> RustlsTlsConnector {
    RustlsTlsConnector::builder()
        .add_root_certificates_pem(server.tls_root_pem().expect("root pem"))
        .expect("root cert")
        .build()
        .expect("tls connector")
}

#[derive(Clone)]
struct ScriptedRaceTcpConnector {
    scripts: Arc<HashMap<SocketAddr, TcpAttemptScript>>,
}

#[derive(Clone, Copy)]
struct TcpAttemptScript {
    actual_addr: SocketAddr,
    delay: Duration,
}

impl ScriptedRaceTcpConnector {
    fn new(entries: impl IntoIterator<Item = (SocketAddr, TcpAttemptScript)>) -> Self {
        Self {
            scripts: Arc::new(entries.into_iter().collect()),
        }
    }
}

impl TcpConnector for ScriptedRaceTcpConnector {
    fn connect(
        &self,
        ctx: CallContext,
        addr: SocketAddr,
        timeout: Option<Duration>,
    ) -> BoxFuture<Result<BoxConnection, WireError>> {
        let script = self
            .scripts
            .get(&addr)
            .copied()
            .expect("missing tcp script");
        Box::pin(async move {
            tokio::time::sleep(script.delay).await;
            TokioTcpConnector
                .connect(ctx, script.actual_addr, timeout)
                .await
        })
    }
}

#[derive(Clone)]
struct MultiAddrResolver {
    map: Arc<HashMap<String, Vec<SocketAddr>>>,
}

impl MultiAddrResolver {
    fn new(entries: impl IntoIterator<Item = (String, Vec<SocketAddr>)>) -> Self {
        Self {
            map: Arc::new(entries.into_iter().collect()),
        }
    }
}

impl DnsResolver for MultiAddrResolver {
    fn resolve(
        &self,
        ctx: CallContext,
        host: String,
        port: u16,
    ) -> BoxFuture<Result<Vec<SocketAddr>, WireError>> {
        let map = self.map.clone();
        Box::pin(async move {
            ctx.listener().dns_start(&ctx, &host, port);
            let addrs = map.get(&host).cloned().ok_or_else(|| {
                WireError::dns(
                    "host not found in resolver map",
                    std::io::Error::new(std::io::ErrorKind::NotFound, "missing host"),
                )
            })?;
            ctx.listener().dns_end(&ctx, &host, &addrs);
            Ok(addrs)
        })
    }
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(10)
        .warm_up_time(Duration::from_millis(250))
        .measurement_time(Duration::from_millis(750));
    targets = perf_baseline
}
criterion_main!(benches);
