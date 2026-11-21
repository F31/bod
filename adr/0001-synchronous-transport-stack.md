# ADR-0001 保留同步传输栈，不引入 async 运行时

- 状态：`accepted`
- 日期：2026-11-21
- 决策者：项目负责人（本次评审后批准）
- 关联：评审报告 CR-003（薄路由层）、评审报告 §五「技术栈偏离」

## 场景

V2.9.6 技术架构规格 §3 原文：

> 核心采用 Rust 2024、Tokio、Axum/Tower、rusqlite、serde、tracing；
> 出站客户端复用 reqwest/rustls。

实际实现：控制面是同步 `std::net::TcpListener` + 手写 HTTP/1.1 解析
（`crates/bod-app/src/host_handshake.rs`），全仓 `tokio` / `async fn` 出现 0 次。
`serde`、`rusqlite` 与规格一致；`tracing` 缺失属另一问题，由 CR-002 单独处理。

按「不允许悄悄漂移」的纪律，这里必须显式记录：**规格的技术栈这一句被有意不采纳**，
而规格 §1—§2 的硬约束（事务边界、事实所有权、确定性内核）全部满足。

## 决策

保留同步栈。不引入 Tokio runtime、不引入 Axum/Tower，HTTP 传输层继续手写。
`tracing` 不在此决策豁免范围内，仍按 CR-002 补齐（它不依赖 async）。

## 理由

1. **规格的硬约束没有一条依赖 async**。真正被 §1—§2 约束的是「控制面共同提交」、
   「事实只有一个所有者」、「确定性内核可重放」——这些在同步栈里已经满足并且有测试
   覆盖（`xtask verify --stage P0` 每次校验）。技术栈是手段，不是架构本质。

2. **`rusqlite` 是同步的，套上 async 只会多一层 `spawn_blocking`**。
   在 async handler 里每访问一次库都要 `spawn_blocking`，跨 await 持有事务句柄会直接
   编译不过，于是事务边界会被迫打散——这恰好破坏规格最在意的「共同提交」。
   为了符合技术栈而削弱正确性，是本末倒置。

3. **当前并发上限由产品形态决定，不由代码决定**。单机单 Company，客户端是本地
   WebView 与 CLI，端点数当前为 3（`/_bod/health`、`/_bod/events`、`/_bod/shutdown`
   加握手）。没有 C10K 场景，`async` 的收益落不到实处。

4. **先解耦再决定，决策成本更低**。CR-003 要抽的薄路由层是
   `(method, path, params, body) → handler`，它把传输细节与业务入口隔开。
   做完之后无论换不换框架，上层都不动——所以「现在不换」不会锁死未来。

## 代价

- **SSE 长连接占用线程**：`GET /_bod/events` 的每个 follow 订阅独占一个线程
  （`host_handshake.rs` 里 `std::thread::spawn`）。线程栈默认 8 MiB 虚拟内存，
  订阅数上百时内存与调度都会吃紧。
- **手写 HTTP 解析要自己守边界**：头长度上限（已设 16 KiB）、请求走私、
  慢连接（已设 5 s 读写超时）。每加一个能力都要重新想一遍这些。
- **无法复用 Axum 生态**：中间件、提取器、测试工具都要自己写。

## 撤销条件（可机械判断，两条同时满足即重评）

1. 单进程并发 SSE 订阅数 **> 50**；
2. 出现 §6 之外的多入口（移动端与 IM 同时在线，导致同一宿主被多类客户端长连）。

届时**优先评估 axum**，不自行扩展手写 HTTP。

判断方式：订阅数由 `event_stream` 的 `StreamStats` / 连接计数读取，不需要人工估计。
在条件触发之前，本 ADR 保持 `accepted`。

## 代码体现

- `crates/bod-app/src/host_handshake.rs`：同步 `TcpListener`、手写请求解析与响应。
- `crates/bod/Cargo.toml` / `crates/bod-app/Cargo.toml`：无 `tokio`、无 `axum`。
- `crates/bod-app/src/event_stream.rs`：订阅走独立线程而非 async task。
