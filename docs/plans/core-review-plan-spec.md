# Core Review Plan Spec

Date: 2026-03-22
Source review: [../core-review.md](../core-review.md)

## Worktree Delivery

| Worktree | Example branch | Scope |
| --- | --- | --- |
| `lifecycle` | `bugfix/lifecycle-timeout-leases` | cancel / lease / background task lifecycle |
| `policy` | `feature/policy-body-redirects` | body semantics / redirect policy / readiness |
| `connectivity` | `feature/connectivity-limits-bindings` | connection limits / connect events / binding contention |
| `proxy` | `feature/proxy-socks-fast-fallback` | SOCKS5 auth / proxy dial fast fallback |
| `pool` | `refactor/pool-eviction-locking` | pool eviction / coalescing / poison-safe locking |
| `protocol` | `bugfix/protocol-reuse-deadline-ordering` | HTTP/1.1 reuse parsing / deadline memory ordering |

Parallel workstreams use `git worktree`.
Default location: `../openwire-worktrees`
Source of truth for repository workflow: `AGENTS.md`

## WS-01 Lifecycle / RAII

| Field | Plan |
| --- | --- |
| Review items | `Future 取消导致 HTTP/2 分配泄漏`; `ResponseLease 缺少 Drop`; `Background connection task 无 shutdown` |
| Target architecture | 连接 ownership 从 `acquire_connection` 到 `ObservedIncomingBody` 必须全程 RAII；runtime spawn 返回可 abort handle；`Client` 持有连接 task registry |
| New types / config | `SpawnHandle` trait object；`ConnectionTaskRegistry`；`SelectedConnectionGuard` 或等价 acquire guard；`ResponseLease::abandon()` |
| Primary files | `crates/openwire-core/src/runtime.rs`; `crates/openwire-core/src/tokio_rt.rs`; `crates/openwire/src/client.rs`; `crates/openwire/src/transport.rs` |
| Function changes | `with_call_deadline`; `acquire_connection`; `bind_fresh_connection`; `send_bound_request`; `ObservedIncomingBody::wrap`; `spawn_http1_task`; `spawn_http2_task` |
| File strategy | 不拆 transport 模块目录；lease/task registry 先留在 `transport.rs` 内部类型，避免一次性改 module 结构 |

## WS-02 Policy / Body Semantics

| Field | Plan |
| --- | --- |
| Review items | `Bridge 层空 body 强制 Content-Length: 0`; `HTTPS -> HTTP 降级重定向`; `FollowUpPolicyService::poll_ready 绕过背压` |
| Target architecture | request body 语义区分 `absent` 与 `explicit empty`；redirect policy 显式建模 downgrade 开关；`FollowUpPolicyService` 与 `TransportService` 的 `poll_ready` 都必须反映真实下游 readiness |
| New types / config | `RequestBody::absent()` / `RequestBody::explicit_empty()` 或等价 body kind；`RedirectPolicyConfig.allow_insecure_redirects`; `ClientBuilder::allow_insecure_redirects(bool)` |
| Primary files | `crates/openwire-core/src/body.rs`; `crates/openwire/src/bridge.rs`; `crates/openwire/src/policy/follow_up.rs`; `crates/openwire/src/client.rs` |
| Function changes | `normalize_body_headers`; `RequestSnapshot::into_redirect_request`; redirect validation helper；`FollowUpPolicyService::poll_ready`; `TransportService::poll_ready` |
| File strategy | 不新增文件；策略和 body 语义分别留在现有 owner 文件中 |

## WS-03 Connectivity / Admission Control

| Field | Plan |
| --- | --- |
| Review items | `无连接并发限制`; `TcpConnector connect 事件生命周期`; `ConnectionBindings 单一 Mutex` |
| Target architecture | transport 层引入 admission control；同时限制建连与 in-flight request；连接事件由单一 orchestration owner 发出；binding map 改为分片结构 |
| New types / config | `TransportConfig.max_connections_total`; `TransportConfig.max_connections_per_host`; `TransportConfig.max_requests_total` 或等价全局请求门限；`TransportConfig.max_requests_per_host` 或等价 host 门限；`ConnectionLimiter`; `ConnectionPermit`; `RequestAdmissionLimiter`; sharded `ConnectionBindings` |
| Primary files | `crates/openwire/src/client.rs`; `crates/openwire/src/transport.rs`; `crates/openwire/src/connection/mod.rs` |
| Function changes | `TransportService::new`; `TransportService::poll_ready`; `execute_exchange`; `acquire_connection`; call entry admission at `Client::execute` / `Call::execute`; `TokioTcpConnector::connect`; direct/proxy connect helpers |
| File strategy | 新增 `crates/openwire/src/connection/limits.rs` 保存 limiter 与 permit，bindings shard 仍留在 `transport.rs` |

## WS-04 Proxy / Route Dialing

| Field | Plan |
| --- | --- |
| Review items | `SOCKS5 仅支持 no-auth`; `代理连接路径不使用 Fast Fallback` |
| Target architecture | proxy credentials 在 `Proxy -> ProxyConfig -> Route` 链路显式传递；fast fallback 分成 `TCP race` 和 `post-connect handshake` 两段 |
| New types / config | `ProxyCredentials`; route-level proxy auth material；`FastFallbackStage` 或等价 dial stage model |
| Primary files | `crates/openwire/src/proxy.rs`; `crates/openwire/src/connection/planning.rs`; `crates/openwire/src/connection/fast_fallback.rs`; `crates/openwire/src/transport.rs` |
| Function changes | proxy URL 解析；`connect_via_http_proxy`; `connect_via_socks_proxy`; `socks5_write_client_greeting`; 新增 RFC 1929 sub-negotiation helper；`FastFallbackDialer` 泛化 |
| File strategy | 不复用 HTTP `proxy_authenticator`；SOCKS5 凭据走 proxy config，而不是 challenge-follow-up 接口 |

## WS-05 Pool / State Management

| Field | Plan |
| --- | --- |
| Review items | `HTTP/2 idle timeout 不驱逐`; `acquire_coalesced 全表扫描`; `Mutex/RwLock 中毒级联 panic` |
| Target architecture | pool eviction 变为协议无关；coalescing 查找基于 secondary index；critical locks 改为 poison-safe primitive |
| New types / config | secondary index keyed by direct target address；统一 lock helper 或 `parking_lot` primitive |
| Primary files | `crates/openwire/src/connection/pool.rs`; `crates/openwire/src/connection/real_connection.rs`; `crates/openwire/src/cookie.rs`; `crates/openwire/src/transport.rs` |
| Function changes | `prune_connections`; `idle_http1_expired` 重命名并泛化；`enforce_max_idle_http1`；`acquire_coalesced`; lock acquisition call sites |
| File strategy | 先不引入新的 pool public config；优先修正现有 `pool_idle_timeout` / `pool_max_idle_per_host` 语义覆盖范围 |

## WS-06 Protocol / Low-level Correctness

| Field | Plan |
| --- | --- |
| Review items | `HTTP/1.1 Connection 头解析错误`; `Body deadline signal 使用 Relaxed` |
| Target architecture | response reuse 判定使用 RFC 9110 token parsing；deadline signal 使用 release/acquire ordering |
| New types / config | 无 |
| Primary files | `crates/openwire/src/transport.rs` |
| Function changes | `http1_response_allows_reuse`; `spawn_body_deadline_signal`; `ObservedIncomingBody::poll_deadline` |
| File strategy | 保持在现有文件；不新增模块 |

## Review Coverage Matrix

| Review item | Workstream |
| --- | --- |
| Future 取消导致 HTTP/2 分配泄漏 | `WS-01` |
| ResponseLease 缺少 Drop | `WS-01` |
| HTTP/1.1 Connection 头检查 | `WS-06` |
| Bridge 空 body / `Content-Length: 0` | `WS-02` |
| 无连接并发限制 | `WS-03` |
| HTTPS -> HTTP 降级重定向 | `WS-02` |
| Background connection task 无 shutdown | `WS-01` |
| SOCKS5 仅支持 no-auth | `WS-04` |
| `connect_start` / `connect_failed` 生命周期 | `WS-03` |
| HTTP/2 idle timeout 不驱逐 | `WS-05` |
| Mutex / RwLock poisoning | `WS-05` |
| `ConnectionBindings` 单一 Mutex | `WS-03` |
| `acquire_coalesced` 全表扫描 | `WS-05` |
| 代理路径不使用 Fast Fallback | `WS-04` |
| `poll_ready` 绕过 Tower 背压 | `WS-02` |
| deadline signal `Relaxed` ordering | `WS-06` |

## Verification Targets

| Area | Verification |
| --- | --- |
| Lifecycle | cancel/timeout 回收测试；HTTP/2 allocation 归零；client drop abort background tasks |
| Policy | GET/HEAD 无 body 不写 `Content-Length`; downgrade redirect 默认拒绝；`poll_ready` 配合 concurrency limiter 生效 |
| Connectivity | per-host / global limit 测试；`connect_failed` 只发一次；bindings 高并发 acquire/release 压测 |
| Proxy | SOCKS5 username/password fixture；proxy IPv4/IPv6 fast fallback fixture |
| Pool | H2 idle eviction；coalescing secondary index correctness；poison recovery tests |
| Protocol | multi-value `Connection` header；comma-separated `close` token；deadline signal ordering smoke tests |

## Doc Sync

| File | Required update |
| --- | --- |
| `docs/core-review.md` | 仅保留问题源与 plan spec 链接 |
| `docs/DESIGN.md` | body header rules；redirect policy；transport config；task lifecycle；admission control |
| `README.md` | body construction示例；proxy capability |
| `AGENTS.md` | worktree usage convention；parallel workstream split rules |
