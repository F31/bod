# Proposal: P2 工作接续

## Why

P1 证明了一个执行者可以完成真实交付，但用户仍会遇到执行 Agent 会话失效、额度耗尽、需要换运行时或需要更换管理负责人的情况。没有分等级的交接、事务化接管与旧命令拒绝机制时，换人会丢失现场、重复执行或把旧管理权带回系统（§18.9—18.13、§41.8）。

## What Changes

- 定义 HandoffPackage 的 L1 资料交接、L2 现场重建与仅在已测组合上的 L3 验证后接续；区分宿主事实与 Agent 叙述，允许部分快照带 missing 与完整性状态。
- 定义切换事务：停止新派发 → 请求旧执行停止 → 捕获/校验现场 → 隔离与对账 → 新 Attempt/fencing token → 重建与预检；旧执行未确认停止时禁止并发写同一现场。
- 定义管理负责人切换：当前 Appointment/authority_epoch、旧管理命令拒绝、员工独立继续、检查点增量读取、已接受成果不重做；组织元数据变化不迁移执行状态。
- 提供公开 `bod conformance` 工具，按适配器版本/协议/平台/能力输出分项报告；基础身份、授权或消息完整性用例失败时禁用派发，不靠 degraded 徽章豁免。
- 非目标：多 Agent 协作工作区与合并队列、共享接口影响图、CI 制品/环境发布、团队 Catalog 与六阶段权限、远程节点与移动审批。

## Capabilities

### New Capabilities

- `handoff-package`: 接续能力等级、规范编码、宿主事实与叙述边界、部分快照与版本绑定。
- `takeover-transaction`: 执行者切换的停止/隔离/对账/预检事务与可接续等级展示。
- `manager-transfer`: 管理负责人委任切换、epoch 与作用域、旧命令拒绝、员工继续与检查点增量。
- `conformance-toolkit`: 公开符合性工具、分项报告、双运行时接续证据与准入规则。

### Modified Capabilities

无。截至 P1 归档（2026-11-20），现行 `openspec/specs/` 含 13 个 capability（P0 9 + P1 4）；本 Change 的 4 个能力与之均不同名，全部以 ADDED 新增，不改写既有规格与历史约定。

## Impact

- 代码：`crates/bod-app/src/execution/`（停止、隔离、检查点）、`crates/bod-app/src/coding/`（现场捕获与重建）、`crates/bod-app/src/governance/`（Appointment/epoch/ManagerTransfer）、`xtask`/CLI（conformance 子命令）。
- 数据：新增 handoff_packages、takeover_transactions、checkpoints、manager_transfers、conformance_reports；沿用 P0 的 effects/reconciliation_cases 与 P1 的 workspaces。
- 验证：绑定 I-46—I-50、I-58—I-65、G-17、G-28—G-30、G-38—G-42、RM03（§39.10—39.11、§39.20）；初始 planned。
- 依赖：P0 租约/Effect Gate/账本与 P1 隔离工作区/ContextBundle 必须可用；不新增常驻服务。
