# Proposal: P0 最小可信核心

## Why

V2.9.6 把 P0 定义为「最小可信核心」（§40.1），出口证据是重复命令、丢回包、崩溃恢复、授权拒绝与空闲负担实测；P0 即需具备最小治理，而不是后期补权限或预算（§40.1、§2.3）。当前仓库 `openspec/` 尚无现行规格，缺少可作为工作包输入与验收基线的确定性行为契约，无法把「已提交代码」与「已通过验证」区分开。

本变更把设计基线中 P0 可执行的行为固定为规格，并给出按垂直切片的实现任务；不宣称任何条目已经实现或已通过测试（§41.1、§39.15）。

## What Changes

- 建立默认本地单宿主模型：一个 Company、Project、Agent、Issue 的最小记录与直接指派路径；不要求 Team、组织图或 WorkPackage 配置即可交办。
- 明确可信宿主为所有业务写入与状态仲裁的唯一主体：权限取交集、当前授权复核、执行端只报告观察；Agent 反馈与自然语言的「授权声明」不改变权限。
- 定义资源外壳与状态所有权：期望 spec 与实际 status 分离、版本号/代数、条件更新、合法停止状态与删除请求语义。
- 定义持久协调循环：wake_receipts 去重、work_queue 代数、租约/fencing token、提交后 outbox、重启后恢复与有限退避。
- 定义 Effect Gate 与幂等：外部动作意图、Dispatched/Unknown 崩溃窗口、未知非幂等动作不自动重发、取消确认与 reconcile。
- 定义预算准入事务与不可变账本：Company→Project→Agent→Run→WorkPackage 同事务复核、C/R/U 互斥记账、Busy 有界重试、预算解释。
- 定义执行端最小协议：describe/probe/start/inspect/observe、分项能力声明、invocation 幂等、Completed 不等于 Accepted；P0 只接一种真实执行配置，第二种留待 P1。
- 定义候选成果、证据与最小接受门：实现者无隐式自批权，接受绑定确定规格/产物/验证修订。
- 定义最小 doctor/对账、备份恢复与清理阻塞语义，以及三核心 crate + Tauri 壳 + xtask 的构建与验证基线。
- 非目标（本 Change 不实现）：多 Agent 协作与 worktree 合并队列、第二执行端与 L2/L3 接续、负责人委任与切换、OpenSpec 影响图、Team Catalog、远程节点/移动、CI 制品与阶段 Manifest 的完整发布链、生产写入与团队权限开放。

## Capabilities

### New Capabilities

- `identity-access`: 本地单用户下 Company/Project/Agent/Issue 的身份、成员与项目授权、最小权限交集、直接指派与拒绝/审计语义。
- `resource-state`: 资源外壳、期望与实际分离、版本/代数、状态机与写入所有权、删除请求与终态语义。
- `coordination-loop`: 持久唤醒回执、资源队列代数与租约、确定性控制器推进、稳定停止状态、重启恢复与积压处理。
- `effect-gate`: 外部副作用意图、幂等键与去重、outbox 提交后派发、Unknown 对账、取消确认与租约 fencing。
- `budget-admission`: 分层预算准入事务、预留/结算/风险占用、并发 Busy 语义、预算解释与默认无隐式付费授权。
- `execution-adapter`: 宿主—执行端最小契约 describe/probe/start/inspect/observe、分项能力声明、invocation 去重与观察语义。
- `artifact-acceptance`: 候选成果修订、证据绑定、最小接受/拒绝权限与不可自批、旧接受不覆盖新修订。
- `recovery-cleanup`: 最小 doctor、备份与恢复、外部动作对账、删除请求/清理阻塞与隔离边界。

### Modified Capabilities

无。`openspec list --specs` 显示项目当前没有现行规格，全部为新增。

## Impact

- 代码与目录：`crates/bod-core`（纯规则、值类型）、`crates/bod-app`（UnitOfWork、SQLite、队列、预算、执行、恢复）、`crates/bod`（CLI/HTTP）、`apps/desktop/src-tauri`（连接原型）、`xtask`（验证索引、迁移 lint、契约生成）。
- 持久结构：`events`、`issues`/`work_package_revisions`/`runs`/`attempts`/`leases`、`effects`/`effect_attempts`/`outbox`/`reconciliation_cases`、`reservations`/`ledger_entries`/`usage_reports`、`wake_receipts`/`work_queue`、`members`/`grants`/`config_revisions`（§33.2、§9.10）。
- 契约：控制 API 的资源与命令入口、错误码/409 冲突语义、执行协议 v1 的最小子集与 conformance fixtures（§15.7）。
- 验证：I-01—I-06、I-09、I-11、I-12、I-68—I-70、G-1、G-2、G-6、G-8、G-10、G-44、G-45 等按 §39.13—39.14 绑定；本 Change 仅建立 planned 绑定，不把计划路径当作通过证据。
- 依赖与边界：SQLite WAL + 单写线程 + outbox；无 Redis、外部 MQ、第二业务数据库写入者、自研模型推理循环（§2.3、§37.13）。
