# Design: P0 最小可信核心

## Context

动机与范围见 `proposal.md`，产品约束来自《bod V2.9.6》§2、§9—§13、§33—§34、§39—§41。仓库当前没有现行 OpenSpec 规格，也没有已核实的工程实现证据；因此本设计描述的是待实现架构，所有测试与门禁初始为 planned（§39.13—39.15）。

P0 只服务本地单用户、单宿主、默认 Company。可信 OS 用户之外不承诺隔离；多 Company 需分离 DB/CAS/凭据，但不抵抗同一 OS 用户（§31.2）。

## Goals / Non-Goals

- 目标：一个确定性的本地宿主，能受理 Issue、原子认领、按预算准入、派发给一个真实本地执行端、登记候选结果并执行最小接受；重复请求、回包丢失、宿主崩溃、权限拒绝有可验证行为。
- 目标：建立 I/G 验证索引与三核心 crate + 桌面壳 + xtask 的依赖方向基线，使后续 P1—P7 Change 能复用同一规格路径。
- 非目标：第二执行端、接续等级与负责人切换、跨模块影响图、worktree 合并队列、Catalog、远程/移动、团队六阶段权限、生产发布与完整 CI 制品链。
- 非目标：把 Fake Executor 当生产智能执行能力；它只用于故障 conformance（§15.7）。

## Decisions

1. **纯决策内核 + 宿主端口。** 领域逻辑以 `decide(state, command) -> events` / `evolve(state, event)` 形式保持可测；时间、随机、持久化、网络与进程由宿主注入。替代方案是在领域对象中直接调用 IO，会破坏重放与确定性测试（§10.1）。
2. **单写 UnitOfWork + BEGIN IMMEDIATE。** 一次接受命令在同一事务完成版本检查、授权、预算、租约、事件与 outbox；提交后才发网络/进程动作。Busy 在请求 deadline 内整体重读重试并返回稳定 request_id。替代方案是各模块自开事务，会造成部分占用与重复扣减（§11.7、§33.3）。
3. **持久队列两结构。** `wake_receipts` 以 (company_id, producer_id, idempotency_key) 做历史去重；`work_queue` 以资源键保存 wanted/claimed/completed_generation、lease 与 next_due_at。进程内通知只加速，不能替代扫描恢复。新唤醒递增 wanted_generation，运行中不创建第二个消费者（§9.10—9.11）。
4. **租约与 fencing token 持久化单调。** 认领在条件更新中唯一成功；旧 token 不能更新宿主状态。租约过期先恢复处理权，若已发起外部动作则查 invocation，不直接重发（§10.6—10.7）。
5. **Effect Gate 与 Unknown 优先对账。** 意图先落盘，逻辑动作键含 Company、目标资源、逻辑动作 ID、授权范围与语义版本；同键不同参数拒绝。回包丢失进入 Dispatched/Unknown，查询或人工裁决，Unknown 不做盲重试（§10.3—10.5）。
6. **预算三量分离。** 可用额 = 限额 − 已确认 C − 有效预留 R − 待对账 U；两级占用互斥转换，超额如实入账并追加调整分录，不截断 actual（§11.3、§11.10）。金额用整数最小计价单位或定点数，不浮点累计。
7. **最小执行协议。** P0 实现 describe/probe/start/inspect/observe 与能力声明；cancel/resume/reconcile/ACK 可标 Unsupported，但界面必须如实显示限制。执行端 Accepted/Completed 与业务 Accepted 分开（§15.2—15.4、§15.8）。
8. **接受证据绑定修订。** Acceptance 记录引用输入基线、工作包修订、产物修订与验证规则；实现者无隐式自批权，退出码 0 不等于 Accepted（§5.6、§8.4、§41.4）。
9. **资源外壳按对象选用。** 需要并发与状态投影的 Run/Attempt 使用 apiVersion/kind、resourceVersion、generation、spec/status/conditions；不可变成果与账本分录只追加。Status 命令不是通用 PATCH（§8.1—8.5）。
10. **默认配置冻结在代码中且可解释。** §37.14 的 busy_timeout_ms、claim_lease_seconds、retry 窗口与缓存上限只能在配置 schema 与 explain 定义后使用；claim lease 不等于 Agent 执行超时，168 小时到期不删除 Unknown 去重（§37.14）。
11. **依赖方向与验证入口。** `bod-core` 不依赖 app/存储/网络；`bod-app` 不反向依赖 CLI/桌面；xtask 承担 verify、migration-check、generate-contracts --check。真实 npm/平台测试只在对应作业运行，不以 headless 结果宣称桌面通过（§37.1—37.3、§37.12）。

## Risks / Trade-offs

- [单写入口成为瓶颈] → 短事务 + 有界队列 + 按 Company 公平调度；只有真实争用实测不达标才评估分片（§11.7、§40.4）。
- [SQLite 无行锁语义] → 不把 FOR UPDATE 假设带入代码；所有准入在同一写线程重读并条件更新，保留并发测试（§11.7）。
- [重启后误判外部执行] → 先 inspect/reconcile，再调整槽位与占用；Unknown 保留风险与现场，不按普通 TTL 淘汰（§9.5、§10.4）。
- [适配器自报能力被当成安全证明] → 分项 probe + 来源/时间 + conformance；强隔离声明必须跑运行器测试，否则降级（§10.2、§10.11）。
- [P0 无真实执行端可用时出口受阻] → Fake Executor 先完成全部协议故障样本，真实适配器接入作为 P0 放行的独立任务，不把 fake 通过写进支持矩阵。
- [规格与实现漂移] → 任务绑定 I/G 与 scenario，验收绑定确定修订；不使用 tasks.md 勾选作为验收证据（§26.6、§39.15）。

## Migration Plan

本 Change 是绿地基线，不涉及旧库升级：从空仓库建立 crate 边界、初始 migration 与验证索引。若后续存在旧样本库，迁移必须登记 migration_id、checksum、依赖、兼容窗口与恢复测试；破坏性 DDL 需显式授权并阻断默认执行（§33.7）。回滚策略为保留迁移前备份与进程/会话核验入口，恢复到新宿主后 Unknown 先对账再开放新派发。

## Open Questions

- P0 具体接入哪一种本机执行端由 conformance 结果和本机已就绪运行时决定；不改变协议契约与任务结构。
- Tauri 壳在 P0 只验证连接/生命周期，发行平台与 WebView 组合的支持矩阵在对应平台测试后单独公布；不改变本 Change 的行为规格。
