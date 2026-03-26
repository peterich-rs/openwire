use std::future::Future;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_channel::mpsc;
use futures_util::future::{AbortHandle, Abortable};
use futures_util::stream::StreamExt;
use hyper::rt::Timer;
use hyper::Uri;
use openwire_core::{
    BoxConnection, CallContext, EstablishmentStage, SharedTimer, TcpConnector, TlsConnector,
    WireError, WireExecutor,
};
use tracing::instrument::WithSubscriber;

use super::{
    ConnectAttemptState, ConnectFailure, ConnectFailureStage, ConnectPlan, Route, RouteFamily,
    RouteKind, RoutePlan,
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

#[derive(Clone)]
pub(crate) struct FastFallbackRuntime {
    pub(crate) executor: Arc<dyn WireExecutor>,
    pub(crate) timer: SharedTimer,
}

impl FastFallbackRuntime {
    pub(crate) fn new(executor: Arc<dyn WireExecutor>, timer: SharedTimer) -> Self {
        Self { executor, timer }
    }
}

#[derive(Clone)]
pub(crate) struct DirectDialDeps {
    pub(crate) runtime: FastFallbackRuntime,
    pub(crate) tcp_connector: Arc<dyn TcpConnector>,
    pub(crate) tls_connector: Option<Arc<dyn TlsConnector>>,
    pub(crate) connect_timeout: Option<Duration>,
}

impl DirectDialDeps {
    pub(crate) fn new(
        executor: Arc<dyn WireExecutor>,
        timer: SharedTimer,
        tcp_connector: Arc<dyn TcpConnector>,
        tls_connector: Option<Arc<dyn TlsConnector>>,
        connect_timeout: Option<Duration>,
    ) -> Self {
        Self {
            runtime: FastFallbackRuntime::new(executor, timer),
            tcp_connector,
            tls_connector,
            connect_timeout,
        }
    }
}

impl FastFallbackDialer {
    pub(crate) async fn dial_route_plan<I, C, CFut, F, FFut>(
        &self,
        ctx: CallContext,
        uri: Uri,
        route_plan: RoutePlan,
        runtime: FastFallbackRuntime,
        connect: C,
        finalize: F,
    ) -> Result<(BoxConnection, FastFallbackOutcome), WireError>
    where
        I: Send + 'static,
        C: Fn(CallContext, Route) -> CFut + Send + Sync + Clone + 'static,
        CFut: Future<Output = Result<I, WireError>> + Send + 'static,
        F: Fn(CallContext, Uri, Route, I) -> FFut + Send + Sync + Clone + 'static,
        FFut: Future<Output = Result<BoxConnection, WireError>> + Send + 'static,
    {
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
            "planned connect routes",
        );

        if route_plan.is_empty() {
            return Err(WireError::route_exhausted("route plan produced no routes"));
        }

        let mut connect_plan = ConnectPlan::from_route_plan(&route_plan);
        let (tx, mut rx) = mpsc::unbounded::<FastFallbackMessage>();
        let mut tasks = Vec::new();
        for index in 0..route_count {
            let attempt = connect_plan
                .attempt(index)
                .expect("connect plan should contain every route")
                .clone();
            let ctx = ctx.clone();
            let uri = uri.clone();
            let tx = tx.clone();
            let connect = connect.clone();
            let finalize = finalize.clone();
            let timer = runtime.timer.clone();
            let (abort, abort_registration) = AbortHandle::new_pair();
            let future = async move {
                let _ = Abortable::new(
                    async move {
                        if attempt.scheduled_after() > Duration::ZERO {
                            timer.sleep(attempt.scheduled_after()).await;
                        }
                        let route_family = route_family_label(attempt.route().family());
                        let _ = tx.unbounded_send(FastFallbackMessage::Started {
                            race_id,
                            route_index: index,
                            route_count,
                            route_family,
                        });

                        let route = attempt.route().clone();
                        let result = match connect(ctx.clone(), route.clone()).await {
                            Ok(intermediate) => finalize(ctx, uri, route, intermediate).await,
                            Err(error) => Err(error),
                        };
                        let _ = tx.unbounded_send(FastFallbackMessage::Finished {
                            route_index: index,
                            result,
                        });
                    },
                    abort_registration,
                )
                .await;
            };
            if let Err(error) = runtime
                .executor
                .spawn(Box::pin(future.with_current_subscriber()))
            {
                abort_all(&mut tasks);
                return Err(error);
            }
            tasks.push(RaceTask { abort });
        }
        drop(tx);

        let mut last_error = None;

        while let Some(message) = rx.next().await {
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
                        connect_plan.promote_winner(route_index);
                        ctx.listener()
                            .connect_race_won(&ctx, race_id, route_index, route_count);
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
                            &mut tasks,
                        );
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
                        let stage = failure_stage(&error);
                        connect_plan.mark_failed(
                            route_index,
                            ConnectFailure::new(stage, error.message().to_owned()),
                        );
                        ctx.listener().connect_race_lost(
                            &ctx,
                            race_id,
                            route_index,
                            route_count,
                            loss_reason(stage),
                        );
                        tracing::debug!(
                            call_id = ctx.call_id().as_u64(),
                            connect_race_id = race_id,
                            route_index,
                            route_count,
                            failure_stage = ?stage,
                            error_kind = %error.kind(),
                            error_message = %error.message(),
                            "fast fallback connect attempt failed",
                        );
                        if !error.is_retryable_establishment() {
                            abort_all(&mut tasks);
                            return Err(error);
                        }
                        last_error = Some(error);
                    }
                },
            }
        }

        abort_all(&mut tasks);
        Err(last_error.unwrap_or_else(|| {
            WireError::route_exhausted("no fast-fallback route could be connected")
        }))
    }

    pub(crate) async fn dial_direct(
        &self,
        ctx: CallContext,
        uri: Uri,
        route_plan: RoutePlan,
        deps: DirectDialDeps,
    ) -> Result<(BoxConnection, FastFallbackOutcome), WireError> {
        self.dial_route_plan(
            ctx,
            uri,
            route_plan,
            deps.runtime.clone(),
            move |ctx, route| {
                let tcp_connector = deps.tcp_connector.clone();
                let connect_timeout = deps.connect_timeout;
                async move {
                    match route.kind() {
                        RouteKind::Direct { target } => {
                            tcp_connector.connect(ctx, *target, connect_timeout).await
                        }
                        other => Err(WireError::internal(
                            format!("unexpected non-direct route in direct dialer: {other:?}"),
                            io::Error::other("unexpected non-direct route"),
                        )),
                    }
                }
            },
            move |ctx, uri, _route, stream| {
                let tls_connector = deps.tls_connector.clone();
                async move { finalize_direct_connection(ctx, uri, stream, tls_connector).await }
            },
        )
        .await
    }
}

async fn finalize_direct_connection(
    ctx: CallContext,
    uri: Uri,
    stream: BoxConnection,
    tls_connector: Option<Arc<dyn TlsConnector>>,
) -> Result<BoxConnection, WireError> {
    if !uri
        .scheme_str()
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https"))
    {
        return Ok(stream);
    }

    let Some(tls_connector) = tls_connector else {
        return Err(WireError::tls(
            "HTTPS requested but no TLS connector is configured",
            io::Error::new(io::ErrorKind::Unsupported, "missing TLS connector"),
        ));
    };

    tls_connector.connect(ctx, uri, stream).await
}

fn failure_stage(error: &WireError) -> ConnectFailureStage {
    match error.establishment_stage() {
        Some(EstablishmentStage::Tls) => ConnectFailureStage::Tls,
        Some(EstablishmentStage::ProtocolBinding) => ConnectFailureStage::ProtocolBinding,
        Some(EstablishmentStage::ProxyTunnel) => ConnectFailureStage::ProxyTunnel,
        _ => ConnectFailureStage::Tcp,
    }
}

fn loss_reason(stage: ConnectFailureStage) -> &'static str {
    match stage {
        ConnectFailureStage::Tcp => "connect_failed",
        ConnectFailureStage::Tls => "tls_failed",
        ConnectFailureStage::ProtocolBinding => "protocol_failed",
        ConnectFailureStage::ProxyTunnel => "proxy_failed",
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

struct RaceTask {
    abort: AbortHandle,
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

fn cleanup_losers(
    ctx: &CallContext,
    connect_plan: &mut ConnectPlan,
    race_id: u64,
    winner_index: usize,
    route_count: usize,
    tasks: &mut [RaceTask],
) {
    let mut canceled_routes = vec![false; route_count];
    for (index, task) in tasks.iter_mut().enumerate() {
        if index != winner_index {
            task.abort.abort();
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

fn abort_all(tasks: &mut [RaceTask]) {
    for task in tasks.iter_mut() {
        task.abort.abort();
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
    use openwire_core::{
        BoxConnection, CallContext, Connected, Connection, ConnectionInfo,
        NoopEventListenerFactory, SharedEventListenerFactory, SharedTimer, TlsConnector, WireError,
        WireErrorKind, WireExecutor,
    };
    use tokio::sync::Notify;

    use super::FastFallbackDialer;
    use crate::connection::{
        Address, AuthorityKey, DefaultRoutePlanner, DirectDialDeps, DnsPolicy, ProtocolPolicy,
        RoutePlan, RoutePlanner, UriScheme,
    };

    #[derive(Clone)]
    struct ScriptedTcpConnector {
        scripts: Arc<HashMap<SocketAddr, TcpScript>>,
        success_notifiers: Arc<HashMap<SocketAddr, Arc<Notify>>>,
        attempts: Arc<AtomicUsize>,
        drops: Arc<AtomicUsize>,
    }

    #[derive(Clone)]
    enum TcpScript {
        Success { delay: Duration },
        Fail { delay: Duration },
    }

    impl ScriptedTcpConnector {
        fn new(scripts: impl IntoIterator<Item = (SocketAddr, TcpScript)>) -> Self {
            Self {
                scripts: Arc::new(scripts.into_iter().collect()),
                success_notifiers: Arc::new(HashMap::new()),
                attempts: Arc::new(AtomicUsize::new(0)),
                drops: Arc::new(AtomicUsize::new(0)),
            }
        }

        fn with_success_notifiers(
            self,
            notifiers: impl IntoIterator<Item = (SocketAddr, Arc<Notify>)>,
        ) -> Self {
            Self {
                success_notifiers: Arc::new(notifiers.into_iter().collect()),
                ..self
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
                .cloned()
                .expect("missing tcp script");
            let success_notifier = self.success_notifiers.get(&addr).cloned();
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
                        let stream = Box::new(TestConnection::new(info, drops)) as BoxConnection;
                        if let Some(success_notifier) = success_notifier {
                            success_notifier.notify_one();
                        }
                        Ok(stream)
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

    #[derive(Clone)]
    enum TlsScript {
        Pass {
            delay: Duration,
            wait_for: Option<Arc<Notify>>,
        },
        Fail {
            delay: Duration,
        },
        FailNonRetryable {
            delay: Duration,
        },
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
                    TlsScript::Pass { delay, wait_for } => {
                        if let Some(wait_for) = wait_for {
                            tokio::time::timeout(Duration::from_secs(1), wait_for.notified())
                                .await
                                .expect("expected a completed loser connection before TLS exit");
                        }
                        tokio::time::sleep(delay).await;
                        ctx.listener().tls_end(&ctx, &host);
                        Ok(stream)
                    }
                    TlsScript::Fail { delay } | TlsScript::FailNonRetryable { delay } => {
                        tokio::time::sleep(delay).await;
                        let error = match script {
                            TlsScript::Fail { .. } => WireError::tls(
                                "scripted tls failure",
                                io::Error::new(io::ErrorKind::ConnectionAborted, "scripted"),
                            ),
                            TlsScript::FailNonRetryable { .. } => WireError::tls_non_retryable(
                                "scripted tls policy failure",
                                io::Error::new(io::ErrorKind::InvalidData, "scripted"),
                            ),
                            TlsScript::Pass { .. } => unreachable!("pass handled above"),
                        };
                        ctx.listener().tls_failed(&ctx, &host, &error);
                        Err(error)
                    }
                }
            })
        }
    }

    struct TestConnection {
        inner: openwire_tokio::TokioIo<tokio::io::DuplexStream>,
        info: ConnectionInfo,
        drops: Arc<AtomicUsize>,
    }

    impl TestConnection {
        fn new(info: ConnectionInfo, drops: Arc<AtomicUsize>) -> Self {
            let (stream, _peer) = tokio::io::duplex(64);
            Self {
                inner: openwire_tokio::TokioIo::new(stream),
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
            Connected::new().info(self.info.clone())
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
        let planner = DefaultRoutePlanner::new(stagger);
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

    fn direct_dial_deps(
        tcp: Arc<ScriptedTcpConnector>,
        tls: Option<Arc<ScriptedTlsConnector>>,
    ) -> DirectDialDeps {
        DirectDialDeps::new(
            Arc::new(openwire_tokio::TokioExecutor::new()) as Arc<dyn WireExecutor>,
            SharedTimer::new(openwire_tokio::TokioTimer::new()),
            tcp,
            tls.map(|tls| tls as Arc<dyn TlsConnector>),
            None,
        )
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
                wait_for: None,
            },
        ]));

        let (stream, outcome) = FastFallbackDialer
            .dial_direct(
                ctx(),
                "https://example.com/".parse().expect("uri"),
                route_plan_with_stagger(Duration::from_millis(1), vec![addr1, addr2]),
                direct_dial_deps(tcp.clone(), Some(tls.clone())),
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
        let loser_connected = Arc::new(Notify::new());
        let tcp = Arc::new(
            ScriptedTcpConnector::new([
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
            ])
            .with_success_notifiers([(addr2, loser_connected.clone())]),
        );
        let tls = Arc::new(ScriptedTlsConnector::new(vec![
            TlsScript::Pass {
                delay: Duration::ZERO,
                wait_for: Some(loser_connected),
            },
            TlsScript::Pass {
                delay: Duration::from_millis(5),
                wait_for: None,
            },
        ]));

        let (stream, _outcome) = FastFallbackDialer
            .dial_direct(
                ctx(),
                "https://example.com/".parse().expect("uri"),
                route_plan_with_stagger(Duration::from_millis(1), vec![addr1, addr2]),
                direct_dial_deps(tcp.clone(), Some(tls.clone())),
            )
            .await
            .expect("dial should succeed");

        drop(stream);
        // Loser may have started TLS before abort; 1 or 2 calls are both valid.
        assert!(tls.calls.load(Ordering::Relaxed) >= 1);
        assert!(tls.calls.load(Ordering::Relaxed) <= 2);
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if tcp.drops.load(Ordering::Relaxed) == 2 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("loser stream should be dropped after winner selection");
    }

    #[tokio::test]
    async fn non_retryable_tls_failure_stops_fast_fallback_continuation() {
        let addr1 = SocketAddr::from((Ipv4Addr::new(192, 0, 2, 21), 443));
        let addr2 = SocketAddr::from((Ipv4Addr::new(192, 0, 2, 22), 443));
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
            TlsScript::FailNonRetryable {
                delay: Duration::from_millis(1),
            },
            TlsScript::Pass {
                delay: Duration::from_millis(1),
                wait_for: None,
            },
        ]));

        let error = match FastFallbackDialer
            .dial_direct(
                ctx(),
                "https://example.com/".parse().expect("uri"),
                route_plan_with_stagger(Duration::from_millis(1), vec![addr1, addr2]),
                direct_dial_deps(tcp.clone(), Some(tls.clone())),
            )
            .await
        {
            Ok(_) => panic!("non-retryable tls failure should stop continuation"),
            Err(error) => error,
        };

        assert_eq!(error.kind(), WireErrorKind::Tls);
        assert!(!error.is_retryable_establishment());
        // Route 2 may enter TLS before route 1's non-retryable failure reaches
        // the controller; one or two TLS starts are both valid.
        let tls_calls = tls.calls.load(Ordering::Relaxed);
        assert!(tls_calls >= 1);
        assert!(tls_calls <= 2);
    }
}
