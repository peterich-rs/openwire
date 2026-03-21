use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use hyper::Uri;
use openwire_core::{BoxConnection, CallContext, TcpConnector, TlsConnector, WireError};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::instrument::WithSubscriber;

use super::{
    ConnectAttemptState, ConnectFailure, ConnectFailureStage, ConnectPlan, RouteFamily, RouteKind,
    RoutePlan,
};

static NEXT_CONNECT_RACE_ID: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FastFallbackOutcome {
    pub(crate) race_id: u64,
    pub(crate) route_count: usize,
    pub(crate) winner_index: usize,
    pub(crate) fast_fallback_enabled: bool,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct FastFallbackDialer;

impl FastFallbackDialer {
    pub(crate) async fn dial_direct(
        &self,
        ctx: CallContext,
        uri: Uri,
        route_plan: RoutePlan,
        tcp_connector: Arc<dyn TcpConnector>,
        tls_connector: Option<Arc<dyn TlsConnector>>,
        connect_timeout: Option<Duration>,
    ) -> Result<(BoxConnection, FastFallbackOutcome), WireError> {
        let route_count = route_plan.len();
        let fast_fallback_enabled = route_count > 1;
        let race_id = next_connect_race_id();
        ctx.listener()
            .route_plan(&ctx, route_count, fast_fallback_enabled);
        tracing::debug!(
            call_id = ctx.call_id().as_u64(),
            route_count,
            fast_fallback_enabled,
            connect_race_id = race_id,
            "planned direct connect routes",
        );

        if route_plan.is_empty() {
            return Err(WireError::connect(
                "route plan produced no direct routes",
                io::Error::new(io::ErrorKind::NotConnected, "empty route plan"),
            ));
        }

        let mut connect_plan = ConnectPlan::from_route_plan(&route_plan);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut handles = Vec::new();
        for index in 0..route_count {
            let attempt = connect_plan
                .attempt(index)
                .expect("connect plan should contain every route")
                .clone();
            let ctx = ctx.clone();
            let tx = tx.clone();
            let tcp_connector = tcp_connector.clone();
            let route_count = route_count;
            handles.push(tokio::spawn(
                async move {
                    if attempt.scheduled_after() > Duration::ZERO {
                        tokio::time::sleep(attempt.scheduled_after()).await;
                    }
                    let route_family = route_family_label(attempt.route().family());
                    let _ = tx.send(FastFallbackMessage::Started {
                        race_id,
                        route_index: index,
                        route_count,
                        route_family,
                    });

                    let result = match attempt.route().kind() {
                        RouteKind::Direct { target } => {
                            tcp_connector.connect(ctx, *target, connect_timeout).await
                        }
                        other => Err(WireError::internal(
                            format!("unexpected non-direct route in direct dialer: {other:?}"),
                            io::Error::other("unexpected non-direct route"),
                        )),
                    };

                    let _ = tx.send(FastFallbackMessage::Finished {
                        route_index: index,
                        result,
                    });
                }
                .with_current_subscriber(),
            ));
        }
        drop(tx);

        let tls_required = uri
            .scheme_str()
            .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https"));
        let mut last_error = None;

        while let Some(message) = rx.recv().await {
            match message {
                FastFallbackMessage::Started {
                    race_id,
                    route_index,
                    route_count,
                    route_family,
                } => {
                    connect_plan.mark_running(route_index);
                    ctx.listener().connect_race_start(
                        &ctx,
                        race_id,
                        route_index,
                        route_count,
                        &route_family,
                    );
                    tracing::debug!(
                        call_id = ctx.call_id().as_u64(),
                        connect_race_id = race_id,
                        route_index,
                        route_count,
                        route_family,
                        "fast fallback connect attempt started",
                    );
                }
                FastFallbackMessage::Finished {
                    route_index,
                    result,
                } => match result {
                    Ok(stream) => {
                        if !tls_required {
                            connect_plan.promote_winner(route_index);
                            ctx.listener().connect_race_won(
                                &ctx,
                                race_id,
                                route_index,
                                route_count,
                            );
                            tracing::debug!(
                                call_id = ctx.call_id().as_u64(),
                                connect_race_id = race_id,
                                route_index,
                                route_count,
                                "fast fallback connect attempt won",
                            );
                            cleanup_losers(
                                &ctx,
                                &mut connect_plan,
                                race_id,
                                route_index,
                                route_count,
                                &mut handles,
                                &mut rx,
                            )
                            .await;
                            return Ok((
                                stream,
                                FastFallbackOutcome {
                                    race_id,
                                    route_count,
                                    winner_index: route_index,
                                    fast_fallback_enabled,
                                },
                            ));
                        }

                        let Some(tls_connector) = tls_connector.as_ref() else {
                            cleanup_losers(
                                &ctx,
                                &mut connect_plan,
                                race_id,
                                route_index,
                                route_count,
                                &mut handles,
                                &mut rx,
                            )
                            .await;
                            return Err(WireError::tls(
                                "HTTPS requested but no TLS connector is configured",
                                io::Error::new(io::ErrorKind::Unsupported, "missing TLS connector"),
                            ));
                        };

                        match tls_connector
                            .connect(ctx.clone(), uri.clone(), stream)
                            .await
                        {
                            Ok(stream) => {
                                connect_plan.promote_winner(route_index);
                                ctx.listener().connect_race_won(
                                    &ctx,
                                    race_id,
                                    route_index,
                                    route_count,
                                );
                                tracing::debug!(
                                    call_id = ctx.call_id().as_u64(),
                                    connect_race_id = race_id,
                                    route_index,
                                    route_count,
                                    "fast fallback connect attempt won",
                                );
                                cleanup_losers(
                                    &ctx,
                                    &mut connect_plan,
                                    race_id,
                                    route_index,
                                    route_count,
                                    &mut handles,
                                    &mut rx,
                                )
                                .await;
                                return Ok((
                                    stream,
                                    FastFallbackOutcome {
                                        race_id,
                                        route_count,
                                        winner_index: route_index,
                                        fast_fallback_enabled,
                                    },
                                ));
                            }
                            Err(error) => {
                                connect_plan.mark_failed(
                                    route_index,
                                    ConnectFailure::new(
                                        ConnectFailureStage::Tls,
                                        error.message().to_owned(),
                                    ),
                                );
                                ctx.listener().connect_race_lost(
                                    &ctx,
                                    race_id,
                                    route_index,
                                    route_count,
                                    "tls_failed",
                                );
                                tracing::debug!(
                                    call_id = ctx.call_id().as_u64(),
                                    connect_race_id = race_id,
                                    route_index,
                                    route_count,
                                    error_kind = %error.kind(),
                                    error_message = %error.message(),
                                    "fast fallback connect attempt lost after tls failure",
                                );
                                last_error = Some(error);
                            }
                        }
                    }
                    Err(error) => {
                        if let Some(addr) =
                            direct_route_addr(connect_plan.attempt(route_index).expect("attempt"))
                        {
                            ctx.listener().connect_failed(&ctx, addr, &error);
                        }
                        connect_plan.mark_failed(
                            route_index,
                            ConnectFailure::new(
                                ConnectFailureStage::Tcp,
                                error.message().to_owned(),
                            ),
                        );
                        tracing::debug!(
                            call_id = ctx.call_id().as_u64(),
                            connect_race_id = race_id,
                            route_index,
                            route_count,
                            error_kind = %error.kind(),
                            error_message = %error.message(),
                            "fast fallback connect attempt failed",
                        );
                        last_error = Some(error);
                    }
                },
            }
        }

        abort_all(&mut handles).await;
        Err(last_error.unwrap_or_else(|| {
            WireError::connect(
                "no fast-fallback route could be connected",
                io::Error::new(io::ErrorKind::NotConnected, "all routes failed"),
            )
        }))
    }
}

enum FastFallbackMessage {
    Started {
        race_id: u64,
        route_index: usize,
        route_count: usize,
        route_family: String,
    },
    Finished {
        route_index: usize,
        result: Result<BoxConnection, WireError>,
    },
}

fn next_connect_race_id() -> u64 {
    NEXT_CONNECT_RACE_ID.fetch_add(1, Ordering::Relaxed)
}

fn route_family_label(family: RouteFamily) -> String {
    match family {
        RouteFamily::Ipv4 => "ipv4".to_owned(),
        RouteFamily::Ipv6 => "ipv6".to_owned(),
    }
}

fn direct_route_addr(attempt: &super::ConnectAttempt) -> Option<std::net::SocketAddr> {
    match attempt.route().kind() {
        RouteKind::Direct { target } => Some(*target),
        _ => None,
    }
}

async fn cleanup_losers(
    ctx: &CallContext,
    connect_plan: &mut ConnectPlan,
    race_id: u64,
    winner_index: usize,
    route_count: usize,
    handles: &mut [JoinHandle<()>],
    rx: &mut mpsc::UnboundedReceiver<FastFallbackMessage>,
) {
    let mut canceled_routes = vec![false; route_count];
    for (index, handle) in handles.iter_mut().enumerate() {
        if index != winner_index {
            handle.abort();
        }
    }
    for index in 0..route_count {
        emit_canceled_loss_if_needed(
            ctx,
            connect_plan,
            race_id,
            winner_index,
            route_count,
            index,
            &mut canceled_routes,
        );
    }
    abort_all(handles).await;

    while let Some(message) = rx.recv().await {
        if let FastFallbackMessage::Finished {
            route_index,
            result: Ok(stream),
        } = message
        {
            drop(stream);
            emit_canceled_loss_if_needed(
                ctx,
                connect_plan,
                race_id,
                winner_index,
                route_count,
                route_index,
                &mut canceled_routes,
            );
        }
    }
}

fn emit_canceled_loss_if_needed(
    ctx: &CallContext,
    connect_plan: &mut ConnectPlan,
    race_id: u64,
    winner_index: usize,
    route_count: usize,
    route_index: usize,
    canceled_routes: &mut [bool],
) {
    if route_index == winner_index || canceled_routes.get(route_index).copied().unwrap_or(false) {
        return;
    }

    let should_emit = match connect_plan
        .attempt(route_index)
        .map(|attempt| attempt.state())
    {
        Some(ConnectAttemptState::Pending | ConnectAttemptState::Running) => {
            let _ = connect_plan.mark_lost(route_index, winner_index);
            true
        }
        Some(ConnectAttemptState::Lost {
            winner_index: current_winner,
        }) if *current_winner == winner_index => true,
        _ => false,
    };

    if should_emit {
        canceled_routes[route_index] = true;
        ctx.listener()
            .connect_race_lost(ctx, race_id, route_index, route_count, "canceled");
    }
}

async fn abort_all(handles: &mut [JoinHandle<()>]) {
    for handle in handles.iter_mut() {
        handle.abort();
    }
    for handle in handles.iter_mut() {
        let _ = handle.await;
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io;
    use std::net::{Ipv4Addr, SocketAddr};
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};
    use std::time::Duration;

    use hyper::Uri;
    use hyper_util::client::legacy::connect::{Connected, Connection};
    use openwire_core::{
        BoxConnection, CallContext, ConnectionInfo, NoopEventListenerFactory,
        SharedEventListenerFactory, WireError,
    };

    use super::FastFallbackDialer;
    use crate::connection::{
        Address, AuthorityKey, DnsPolicy, ProtocolPolicy, RoutePlan, RoutePlanner, UriScheme,
    };

    #[derive(Clone)]
    struct ScriptedTcpConnector {
        scripts: Arc<HashMap<SocketAddr, TcpScript>>,
        attempts: Arc<AtomicUsize>,
        drops: Arc<AtomicUsize>,
    }

    #[derive(Clone, Copy)]
    enum TcpScript {
        Success { delay: Duration },
        Fail { delay: Duration },
    }

    impl ScriptedTcpConnector {
        fn new(scripts: impl IntoIterator<Item = (SocketAddr, TcpScript)>) -> Self {
            Self {
                scripts: Arc::new(scripts.into_iter().collect()),
                attempts: Arc::new(AtomicUsize::new(0)),
                drops: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl openwire_core::TcpConnector for ScriptedTcpConnector {
        fn connect(
            &self,
            ctx: CallContext,
            addr: SocketAddr,
            _timeout: Option<Duration>,
        ) -> openwire_core::BoxFuture<Result<BoxConnection, WireError>> {
            let script = self
                .scripts
                .get(&addr)
                .copied()
                .expect("missing tcp script");
            let attempts = self.attempts.clone();
            let drops = self.drops.clone();
            Box::pin(async move {
                attempts.fetch_add(1, Ordering::Relaxed);
                tokio::time::sleep(match script {
                    TcpScript::Success { delay } | TcpScript::Fail { delay } => delay,
                })
                .await;
                ctx.listener().connect_start(&ctx, addr);
                match script {
                    TcpScript::Success { .. } => {
                        let info = ConnectionInfo {
                            id: openwire_core::next_connection_id(),
                            remote_addr: Some(addr),
                            local_addr: None,
                            tls: false,
                        };
                        ctx.listener().connect_end(&ctx, info.id, addr);
                        Ok(Box::new(TestConnection::new(info, drops)) as BoxConnection)
                    }
                    TcpScript::Fail { .. } => {
                        let error = WireError::connect(
                            "scripted connect failure",
                            io::Error::new(io::ErrorKind::ConnectionRefused, "scripted"),
                        );
                        ctx.listener().connect_failed(&ctx, addr, &error);
                        Err(error)
                    }
                }
            })
        }
    }

    #[derive(Clone)]
    struct ScriptedTlsConnector {
        scripts: Arc<Mutex<Vec<TlsScript>>>,
        calls: Arc<AtomicUsize>,
    }

    #[derive(Clone, Copy)]
    enum TlsScript {
        Pass { delay: Duration },
        Fail { delay: Duration },
    }

    impl ScriptedTlsConnector {
        fn new(scripts: Vec<TlsScript>) -> Self {
            Self {
                scripts: Arc::new(Mutex::new(scripts)),
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl openwire_core::TlsConnector for ScriptedTlsConnector {
        fn connect(
            &self,
            ctx: CallContext,
            uri: Uri,
            stream: BoxConnection,
        ) -> openwire_core::BoxFuture<Result<BoxConnection, WireError>> {
            let scripts = self.scripts.clone();
            let calls = self.calls.clone();
            Box::pin(async move {
                calls.fetch_add(1, Ordering::Relaxed);
                let host = uri.host().unwrap_or("example.com").to_owned();
                let script = scripts.lock().expect("tls scripts").remove(0);
                ctx.listener().tls_start(&ctx, &host);
                match script {
                    TlsScript::Pass { delay } => {
                        tokio::time::sleep(delay).await;
                        ctx.listener().tls_end(&ctx, &host);
                        Ok(stream)
                    }
                    TlsScript::Fail { delay } => {
                        tokio::time::sleep(delay).await;
                        let error = WireError::tls(
                            "scripted tls failure",
                            io::Error::new(io::ErrorKind::ConnectionAborted, "scripted"),
                        );
                        ctx.listener().tls_failed(&ctx, &host, &error);
                        Err(error)
                    }
                }
            })
        }
    }

    struct TestConnection {
        inner: openwire_core::TokioIo<tokio::io::DuplexStream>,
        info: ConnectionInfo,
        drops: Arc<AtomicUsize>,
    }

    impl TestConnection {
        fn new(info: ConnectionInfo, drops: Arc<AtomicUsize>) -> Self {
            let (stream, _peer) = tokio::io::duplex(64);
            Self {
                inner: openwire_core::TokioIo::new(stream),
                info,
                drops,
            }
        }
    }

    impl Drop for TestConnection {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::Relaxed);
        }
    }

    impl Connection for TestConnection {
        fn connected(&self) -> Connected {
            Connected::new().extra(self.info.clone())
        }
    }

    impl hyper::rt::Read for TestConnection {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: hyper::rt::ReadBufCursor<'_>,
        ) -> Poll<Result<(), io::Error>> {
            Pin::new(&mut self.get_mut().inner).poll_read(cx, buf)
        }
    }

    impl hyper::rt::Write for TestConnection {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<Result<usize, io::Error>> {
            Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
            Pin::new(&mut self.get_mut().inner).poll_flush(cx)
        }

        fn poll_shutdown(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Result<(), io::Error>> {
            Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
        }
    }

    fn ctx() -> CallContext {
        let factory: SharedEventListenerFactory = Arc::new(NoopEventListenerFactory);
        let request = http::Request::builder()
            .uri("http://example.com/")
            .body(openwire_core::RequestBody::empty())
            .expect("request");
        CallContext::from_factory(&factory, &request, None)
    }

    fn route_plan_with_stagger(stagger: Duration, addrs: Vec<SocketAddr>) -> RoutePlan {
        let planner = RoutePlanner::new(stagger);
        let address = Address::new(
            UriScheme::Https,
            AuthorityKey::new("example.com", 443),
            None,
            Some(crate::connection::TlsIdentity::new("example.com")),
            ProtocolPolicy::Http1OrHttp2,
            DnsPolicy::System,
        );
        planner.plan_direct(address, addrs)
    }

    #[tokio::test]
    async fn tls_failure_promotes_later_route_and_only_calls_tls_for_candidates() {
        let addr1 = SocketAddr::from((Ipv4Addr::new(192, 0, 2, 1), 443));
        let addr2 = SocketAddr::from((Ipv4Addr::new(192, 0, 2, 2), 443));
        let tcp = Arc::new(ScriptedTcpConnector::new([
            (
                addr1,
                TcpScript::Success {
                    delay: Duration::ZERO,
                },
            ),
            (
                addr2,
                TcpScript::Success {
                    delay: Duration::from_millis(2),
                },
            ),
        ]));
        let tls = Arc::new(ScriptedTlsConnector::new(vec![
            TlsScript::Fail {
                delay: Duration::from_millis(5),
            },
            TlsScript::Pass {
                delay: Duration::from_millis(1),
            },
        ]));

        let (stream, outcome) = FastFallbackDialer
            .dial_direct(
                ctx(),
                "https://example.com/".parse().expect("uri"),
                route_plan_with_stagger(Duration::from_millis(1), vec![addr1, addr2]),
                tcp.clone(),
                Some(tls.clone()),
                None,
            )
            .await
            .expect("dial should recover after tls failure");

        drop(stream);
        assert_eq!(outcome.route_count, 2);
        assert_eq!(outcome.winner_index, 1);
        assert_eq!(tcp.attempts.load(Ordering::Relaxed), 2);
        assert_eq!(tls.calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn cleanup_drops_completed_loser_streams_after_winner_is_selected() {
        let addr1 = SocketAddr::from((Ipv4Addr::new(192, 0, 2, 11), 443));
        let addr2 = SocketAddr::from((Ipv4Addr::new(192, 0, 2, 12), 443));
        let tcp = Arc::new(ScriptedTcpConnector::new([
            (
                addr1,
                TcpScript::Success {
                    delay: Duration::ZERO,
                },
            ),
            (
                addr2,
                TcpScript::Success {
                    delay: Duration::from_millis(2),
                },
            ),
        ]));
        let tls = Arc::new(ScriptedTlsConnector::new(vec![TlsScript::Pass {
            delay: Duration::from_millis(5),
        }]));

        let (stream, _outcome) = FastFallbackDialer
            .dial_direct(
                ctx(),
                "https://example.com/".parse().expect("uri"),
                route_plan_with_stagger(Duration::from_millis(1), vec![addr1, addr2]),
                tcp.clone(),
                Some(tls.clone()),
                None,
            )
            .await
            .expect("dial should succeed");

        drop(stream);
        assert_eq!(tls.calls.load(Ordering::Relaxed), 1);
        assert!(
            tcp.drops.load(Ordering::Relaxed) >= 2,
            "winner + loser streams should be dropped"
        );
    }
}
