# Core Review — 核心链路实现审查

审查范围：`Client::execute` 到 transport 层的完整请求链路，涵盖连接管理、协议绑定、策略执行、异步安全性等。
审查基线：main 分支 commit 9abcfc1。
关联规划：[`docs/plans/core-review-plan-spec.md`](plans/core-review-plan-spec.md)

---

## [P0] Future 取消导致 HTTP/2 连接分配计数永久泄漏

`with_call_deadline` 使用 `futures_util::future::select` 实现 call timeout：

```rust
match select(future, sleep).await {
    Either::Left((result, _sleep)) => result,
    Either::Right((_ready, _future)) => Err(WireError::timeout(...)),
}
```

`crates/openwire/src/client.rs:441-445`

当 timeout 先触发时，`_future`（包含整个请求执行链）被 drop。如果此时正处于 `execute_exchange` 中 `acquire_connection` 返回之后、`ObservedIncomingBody::wrap` 之前的阶段，`SelectedConnection` 被 drop，其中的 `AcquiredBinding` 和 `RealConnection` 随之销毁。

对 HTTP/2 的影响路径：

1. `bind_fresh_connection` 中，`connection.try_acquire()` 将 `allocations` 从 0 增加到 1（`crates/openwire/src/connection/real_connection.rs:136`）
2. `ConnectionBindings::acquire` 对 HTTP/2 是 clone sender（`crates/openwire/src/transport.rs:1220-1221`），原始 sender 留在 binding map 中
3. Future 被 cancel 后，`SelectedConnection` 被 drop，但没有任何代码调用 `connection.release()`
4. `allocations` 永久保持在 1，`idle_since` 永远不会被设置

后果：该连接在池中永远处于 `InUse { allocations: 1 }` 状态。虽然 HTTP/2 允许多路复用所以后续请求仍然可以使用该连接（`try_acquire` 对 HTTP/2 不限制 allocations>0），但：
- 连接永远无法进入 idle 状态，`completed_exchanges` 的计数永远偏移
- 如果所有真实请求释放后 allocations=1（phantom），pool stats 持续显示 in_use
- 每次 timeout 取消都累加一个 phantom allocation，长期运行下逐渐恶化

对 HTTP/1 的影响较小：sender 是 take 语义（`crates/openwire/src/transport.rs:1199`），被 drop 后 hyper connection task 会检测到并退出，background task 负责从 pool 和 bindings 清理。

根本原因：`SelectedConnection` 和 `BoundResponse` 都没有实现 `Drop`，从 `try_acquire()` 到 `ObservedIncomingBody::wrap()` 之间存在一段无 RAII 保护的资源持有窗口。

修复方向：为 `SelectedConnection` 实现 `Drop`，在 drop 时调用 `connection.release()` 并清理 binding；或引入一个 RAII guard 覆盖 acquire→wrap 阶段。

---

## [P0] ResponseLease 缺少 Drop 实现，timeout 时连接泄漏

`ResponseLease` 是一个持有连接资源的 enum，但没有 `Drop` 实现：

```rust
enum ResponseLease {
    Http1 {
        connection: crate::connection::RealConnection,
        bindings: Arc<ConnectionBindings>,
        sender: http1::SendRequest<RequestBody>,
        reusable: bool,
    },
    Http2 {
        connection: crate::connection::RealConnection,
    },
}
```

`crates/openwire/src/transport.rs:1275-1285`

`ObservedIncomingBody` 实现了 `Drop`（line 1850-1854），但 `ResponseLease` 本身没有。在 `execute_exchange` 中存在一个窗口期：

```rust
let response = send_bound_request(...).await?;     // BoundResponse 持有 ResponseLease
// ... 多行代码处理 connection_info、listener 事件 ...
let body = ObservedIncomingBody::wrap(             // ResponseLease 转移到 body wrapper
    ...
    Some(response.release),
    ...
);
```

`crates/openwire/src/transport.rs:1333-1368`

如果 `with_call_deadline` 的 timeout 恰好在 `send_bound_request` 返回后、`ObservedIncomingBody::wrap` 之前触发，`BoundResponse` 被 drop。此时 `ResponseLease` 被直接 drop，其中的 HTTP/1 sender、connection、bindings 引用全部丢失，但连接池中的 `RealConnection` 仍标记为 InUse。

对 HTTP/1：sender drop 后 hyper background task 最终会清理（临时泄漏）。
对 HTTP/2：与上一个 P0 问题相同，永久 phantom allocation。

这与上一个问题是同一根本原因的不同触发点。

修复方向：为 `ResponseLease` 实现 `Drop`，在未被 `ObservedIncomingBody` 接管时执行清理（等价于 `abandon_response_lease` 逻辑）。

---

## [P0] HTTP/1.1 Connection 头检查不符合 RFC 9110，可导致连接错误复用

`http1_response_allows_reuse` 判断连接是否可复用时，只取了第一个 `Connection` header 的值，并用全字符串匹配 `"close"`：

```rust
!response.headers().get("connection").is_some_and(|value| {
    value.to_str().map(|value| value.eq_ignore_ascii_case("close")).unwrap_or(false)
})
```

`crates/openwire/src/transport.rs:2029-2034`

两个问题：

1. `HeaderMap::get()` 只返回第一个值。如果服务器发送了多个 `Connection` header（例如 `Connection: keep-alive` 和 `Connection: close`），只有第一个被检查，第二个 `close` 被忽略。
2. RFC 9110 Section 7.6.1 规定 Connection 的值是逗号分隔的列表（如 `Connection: keep-alive, close`）。当前代码对整个值做 `eq_ignore_ascii_case("close")`，不会匹配 `"keep-alive, close"` 这种合法格式。

后果：当服务器明确要求关闭连接时，客户端仍会将该 HTTP/1.1 连接放回池中复用，可能导致下一个请求读到上一个响应的残留数据，或在已关闭的 TCP 连接上发送请求。

修复方向：遍历 `get_all("connection")` 的所有值，对每个值按逗号分割并 trim 后逐项匹配 `"close"`。

---

## [P0] Bridge 层无条件为空 body 设置 Content-Length: 0，影响 GET/HEAD 请求

`normalize_body_headers` 根据 `replayable_len` 来决定是否设置 `Content-Length`：

```rust
match replayable_len {
    Some(len) => {
        let value = HeaderValue::from_str(&len.to_string())...;
        headers.insert(CONTENT_LENGTH, value);
        headers.remove(TRANSFER_ENCODING);
    }
    ...
}
```

`crates/openwire/src/bridge.rs:64-69`

`RequestBody::empty()` 的 `replayable_len` 为 `Some(0)`（见 `crates/openwire-core/src/body.rs:28-31`）。这意味着所有使用默认空 body 的 GET、HEAD、DELETE 请求都会被强制加上 `Content-Length: 0` header。

RFC 9110 Section 8.6：

> A user agent SHOULD NOT send a Content-Length header field in a request containing no content.

部分服务器和代理对 GET 携带 `Content-Length: 0` 的请求会返回 400 或行为异常。OkHttp 的 BridgeInterceptor 只在 `body != null` 时才设置 Content-Length。当前实现无法区分 "无 body" 和 "空 body"。

修复方向：当 `replayable_len == Some(0)` 且请求方法为 GET/HEAD 时，跳过 Content-Length 设置；或在 `RequestBody` 中引入 "absent" 与 "empty" 的区分。

---

## [P1] 无连接并发限制，突发流量可无限创建连接

`Client` 和 `TransportService` 没有任何机制限制：
- 单个 host 的最大连接数
- 全局最大连接数
- 并发请求数

每次 `execute_exchange` 在 pool miss 时直接调用 `bind_fresh_connection` 创建新连接：

```rust
let selected = self
    .acquire_connection(&prepared, &request, ctx.clone(), ...)
    .await?;
```

`crates/openwire/src/transport.rs:1330-1332`

`acquire_connection` 在 pool miss 或 binding busy/stale 时，无条件进入 `bind_fresh_connection`（line 1402）。没有等待队列、没有信号量、没有 pending 请求排队。

后果：如果应用层并发发出 10,000 个请求到不同的 host，会同时创建 10,000 个 TCP 连接，可能耗尽文件描述符（默认 1024 on Linux）、引发 OS 级 `EMFILE` 错误。即使是同一 host，如果 HTTP/1 的请求速率超过单连接的处理速率，也会不断创建新连接。

OkHttp 通过 `ConnectionPool.maxIdleConnections` 和 `Dispatcher.maxRequestsPerHost` 限制并发。当前缺少等价机制。

修复方向：引入 per-host 连接上限和全局连接上限，超出时排队等待；可参照 Tower 的 `ConcurrencyLimit` 或 `Buffer` 层。

---

## [P1] HTTPS 到 HTTP 的降级重定向允许 307/308 保留请求体明文传输

`into_redirect_request` 处理重定向时，对 307/308 状态码保留原始方法和请求体：

```rust
let preserve_body = matches!(
    status,
    StatusCode::TEMPORARY_REDIRECT | StatusCode::PERMANENT_REDIRECT
);
```

`crates/openwire/src/policy/follow_up.rs:400-403`

虽然跨 origin 重定向会移除 `AUTHORIZATION` 和 `COOKIE` header（line 425-428），但请求体本身被完整保留。如果从 `https://api.example.com` 307 重定向到 `http://api.example.com`，POST body 中的敏感数据（如密码、token、支付信息）将以明文 HTTP 传输。

`validate_request`（line 449-466）只校验 scheme 是 http 或 https，不阻止降级。没有任何环节检查 scheme 降级并阻止或警告。

修复方向：默认拒绝 HTTPS→HTTP 的 307/308 重定向（或至少丢弃 body），提供 `allow_insecure_redirect` 配置项给显式授权场景。

---

## [P1] Background connection task 无 shutdown 机制，Client drop 后任务持续运行

`spawn_http1_task` 和 `spawn_http2_task` 通过 `Runtime::spawn` 发射后不管：

```rust
self.runtime.spawn(Box::pin(
    async move {
        let result = task.await;
        bindings.remove(connection_id);
        let _ = pool.remove(connection_id);
        ...
    }
    .instrument(span),
))
```

`crates/openwire/src/transport.rs:1534-1548, 1560-1574`

`Runtime::spawn` 返回 `Result<(), WireError>`（`crates/openwire-core/src/runtime.rs:6`），`JoinHandle` 被丢弃，没有任何机制可以取消这些 task。spawned task 通过 Arc 持有 `ConnectionBindings` 和 `ConnectionPool`，只要 task 活着这些资源就无法释放。

后果：
1. `Client` 被 drop 后，所有已建立连接的 background task 持续运行直到服务端关闭连接
2. 如果服务端使用长 keep-alive（如 HTTP/2 默认不超时），这些 task 可能运行数小时
3. 每个 task 持有 `Arc<ConnectionBindings>` 和 pool clone，阻止内存释放
4. 无法实现 graceful shutdown — 无论是 Client 级别还是应用级别

修复方向：`Runtime::spawn` 应返回可 cancel 的 handle；`TransportService` 或 `Client` 维护一个 task tracker（如 `tokio_util::task::TaskTracker`），在 drop 时 abort 所有 background tasks。

---

## [P1] SOCKS5 代理仅支持无认证模式，缺失 RFC 1929 用户名密码认证

`socks5_write_client_greeting` 只发送一种认证方法：

```rust
stream.write_all(&[0x05, 0x01, 0x00]).await
```

`crates/openwire/src/transport.rs:769`

0x00 表示 "NO AUTHENTICATION REQUIRED"。当代理服务器只支持用户名/密码认证（0x02，RFC 1929）时，会返回 0xFF，客户端报错 "SOCKS5 proxy does not support no-auth authentication"（line 798-801），但实际是客户端不支持认证。

`ConnectorStack` 已有 `proxy_authenticator` 字段（line 187），但 SOCKS5 路径完全未使用它。

修复方向：在 greeting 中增加 0x02 方法，当代理选择 0x02 时执行 RFC 1929 子协商，利用已有的 `proxy_authenticator` 接口获取凭据。

---

## [P1] TcpConnector 的 connect_start/connect_failed 事件生命周期不对称

`TokioTcpConnector::connect` 在所有路径上都发出 `connect_start` 事件：

```rust
ctx.listener().connect_start(&ctx, addr);  // line 88
```

但 `connect_failed` 只在超时路径上发出：

```rust
Err(_error) => {
    let error = WireError::connect_timeout(...);
    ctx.listener().connect_failed(&ctx, addr, &error);  // line 98
    return Err(error);
}
```

`crates/openwire/src/transport.rs:88-105`

当 TCP 连接因拒绝（ConnectionRefused）、网络不可达等原因失败时，`connect_start` 已发出但 `connect_failed` 未发出。尽管上层调用者（fast_fallback.rs:266、connect_via_http_forward_proxy:325）会补发该事件，但事件生命周期一致性应由 connector 层自身保证。

修复方向：在 `TokioTcpConnector::connect` 中对所有失败路径统一发出 `connect_failed`。

---

## [P1] HTTP/2 连接不受 idle timeout 驱逐，可能无限驻留连接池

连接池驱逐逻辑只检查 HTTP/1 连接的空闲超时：

```rust
fn idle_http1_expired(connection: &RealConnection, timeout: Duration) -> bool {
    if connection.protocol() != ConnectionProtocol::Http1 {
        return false;  // HTTP/2 永远不过期
    }
    ...
}
```

`crates/openwire/src/connection/pool.rs:251-261`

同时，`enforce_max_idle_http1` 也只约束 HTTP/1 的最大空闲数（`pool.rs:343`）。

如果未配置 `http2_keep_alive_interval`（默认未配置），HTTP/2 连接在服务端关闭后仍驻留池中。在半开连接或网络分区场景下，连接可能在池中无限期存活。

修复方向：为 HTTP/2 连接增加独立的空闲超时机制，或定期通过 `sender.is_closed()` 主动探测。

---

## [P1] 连接池和连接状态的 Mutex 中毒会导致级联 panic

整个连接池和连接状态管理使用 `std::sync::Mutex`，并通过 `.expect()` 处理锁获取：

```rust
let mut pool = self.connections.lock().expect("connection pool lock");
```

`crates/openwire/src/connection/pool.rs:54`（以及 pool.rs 和 real_connection.rs 中的所有 `.lock().expect()` 调用）

Cookie jar 同样使用 `RwLock` + `.expect()`：

```rust
self.0.write().expect("cookie store lock")
```

`crates/openwire/src/cookie.rs:48, 63, 69`

如果任何线程在持有锁期间 panic，锁进入 poisoned 状态，此后所有 `.expect()` 调用都会 panic，导致级联故障。

修复方向：使用 `parking_lot::Mutex`/`RwLock`（不支持 poisoning），或用 `.unwrap_or_else(|e| e.into_inner())` 恢复。

---

## [P2] ConnectionBindings 单一 Mutex 序列化所有连接的 acquire/release

所有连接的 binding 操作共享一个 `Mutex<HashMap<ConnectionId, ConnectionBinding>>`：

```rust
#[derive(Clone, Default)]
struct ConnectionBindings {
    inner: Arc<Mutex<HashMap<ConnectionId, ConnectionBinding>>>,
}
```

`crates/openwire/src/transport.rs:1130-1132`

每个请求的连接获取（`acquire`）、HTTP/1 释放（`release_http1`）、清理（`remove`）都需要锁定这个 Mutex。在高并发场景下（例如 HTTP/2 多路复用多个 stream），所有 stream 的 acquire 和 release 串行化经过同一把锁，成为吞吐瓶颈。

修复方向：按 ConnectionId 分片（sharded map），或使用 `DashMap` 等 concurrent map 替代。

---

## [P2] acquire_coalesced 在全局池锁内遍历所有地址条目

`acquire_coalesced` 获取池锁后，遍历所有地址键和连接：

```rust
let mut pool = self.connections.lock().expect("connection pool lock");
let keys = pool.keys().cloned().collect::<Vec<_>>();
for key in keys {
    let Some(connections) = pool.get_mut(&key) else { continue; };
    prune_connections(&self.settings, connections);
    candidates.extend(
        connections.iter()
            .filter(|connection| can_coalesce(connection, address, route_plan))
            .cloned(),
    );
}
```

`crates/openwire/src/connection/pool.rs:75-97`

O(N*M) 操作全程持有 Mutex（N=地址条目数，M=每条目连接数）。

修复方向：维护按端口/IP 索引的辅助映射降为局部查找。

---

## [P2] 代理连接路径不使用 Fast Fallback，只做串行尝试

`connect_via_http_proxy` 和 `connect_via_socks_proxy` 对代理地址做串行尝试：

```rust
for route in route_plan.iter() {
    match deps.tcp_connector.connect(ctx.clone(), addr, deps.connect_timeout).await {
        Ok(stream) => { ... }
        Err(error) => { last_error = Some(error); }
    }
}
```

`crates/openwire/src/transport.rs:352-396`（CONNECT proxy），`transport.rs:412-462`（SOCKS proxy）

直连路径使用 `FastFallbackDialer` 实现 RFC 8305 并发竞赛，代理路径完全串行。无 `connect_timeout` 时可能等待 OS TCP 超时（30-120s）。

修复方向：将 `FastFallbackDialer` 泛化为对所有 route kind 可用。

---

## [P2] FollowUpPolicyService::poll_ready 绕过 Tower 背压协议

`FollowUpPolicyService` 的 `poll_ready` 无条件返回 Ready：

```rust
fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
    Poll::Ready(Ok(()))
}
```

`crates/openwire/src/policy/follow_up.rs:79-81`

`TransportService::poll_ready` 也是同样的模式（`transport.rs:1583-1584`）。实际的 readiness 在 `call` 内部通过 clone + `ready().await` 检查。

Tower 的 Service contract 要求 `poll_ready` 返回 Ready 意味着下一次 `call` 可以立即执行。当前实现 bypass 了整条链的背压传播。如果 Tower 中间件（如 `tower::limit::ConcurrencyLimit`）被插入到链中，它的 `poll_ready` 会被忽略，限流失效。

修复方向：`poll_ready` 应委托给内部 service 的 `poll_ready`；或在文档中明确说明此设计决策及其限制。

---

## [P2] Body deadline signal 使用 Relaxed 内存序，在弱序架构上存在理论风险

deadline 超时信号的存储和加载使用 `Ordering::Relaxed`：

```rust
// 写入端 (spawned task)
signal_task.expired.store(true, Ordering::Relaxed);
signal_task.waker.wake();

// 读取端 (poll_deadline)
if !deadline_signal.expired.load(Ordering::Relaxed) {
    deadline_signal.waker.register(cx.waker());
    if !deadline_signal.expired.load(Ordering::Relaxed) {
        return Poll::Pending;
    }
}
```

`crates/openwire/src/transport.rs:1714-1718, 1843-1844`

虽然 `AtomicWaker` 的 `register`/`wake` 内部使用 `AcqRel` 序，double-check 模式覆盖了大部分竞态，但严格来说 `Relaxed` 不建立跨线程 happens-before 关系。在 ARM 等弱序架构上，`expired` 的 `true` 理论上可能延迟可见。

修复方向：写入改为 `Ordering::Release`，读取改为 `Ordering::Acquire`。
