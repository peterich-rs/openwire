use std::time::Duration;

use criterion::{criterion_group, criterion_main, Criterion, Throughput};
use futures_util::future::try_join_all;
use http::Request;
use openwire::{Client, RequestBody, RustlsTlsConnector};
use openwire_test::{ok_text, spawn_http1, spawn_https_http2, TestServer};
use tokio::runtime::Runtime;

fn perf_baseline(c: &mut Criterion) {
    let runtime = Runtime::new().expect("runtime");

    let http1_server = runtime.block_on(spawn_http1(|_request| async move { ok_text("bench") }));
    let h2_server = runtime.block_on(spawn_https_http2(
        |_request| async move { ok_text("bench") },
    ));

    let http1_client = Client::builder().build().expect("http1 client");
    let h2_client = Client::builder()
        .tls_connector(tls_connector(&h2_server))
        .build()
        .expect("h2 client");

    let http1_url = http1_server.http_url("/bench-http1");
    let h2_url = format!("https://localhost:{}/bench-h2", h2_server.addr().port());

    runtime.block_on(async {
        single_get(&http1_client, &http1_url).await;
        single_get(&h2_client, &h2_url).await;
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

criterion_group! {
    name = benches;
    config = Criterion::default()
        .sample_size(10)
        .warm_up_time(Duration::from_millis(250))
        .measurement_time(Duration::from_millis(750));
    targets = perf_baseline
}
criterion_main!(benches);
