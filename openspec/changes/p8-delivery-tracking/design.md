# Design: P8 双入口交付

## Context

动机见 `proposal.md`。约束来自三处，任何设计都不能绕过：

- V2.9.8 §11：两条入口共同使用覆盖投影，同一组有效证据应得出同一结论；路径在 OpenAPI 冻结时定稿。
- §5 单写事务：关联来源、目标与回执意图在**现有 Company 单写事务**内提交，不新建第二个写入者。
- 仓库现状：`bod-app::coverage_receipt::judge_coverage(&CoverageInput) -> CoverageVerdict` 已是纯函数，
  结论可复用；但它是当前 15 个零引用模块之一，只被漂移检查「提到」，没有真实调用方。

## Goals / Non-Goals

- 目标：外部导入能独立成立（无内部 Run/Attempt/审批），且与内部研发路径给出同一覆盖结论。
- 目标：重复导入不产生重复回执、不覆盖已登记的外部来源；跨项目同名 PR、跨客户同名环境分别解析。
- 目标：草稿保存与发送彻底分离；证据或授权变化后旧草稿不能发出。
- 非目标：模型调用、渠道 SDK 抽象、多租户权限模型（P5）、远程节点（P6）。

## Decisions

### D1 覆盖判定只做一次，两条入口共用

`judge_coverage` 作为**唯一**判定入口，两条入口都先把各自的原始事实规范化成 `CoverageInput`：

| 入口 | 规范化时补齐 | 不许做的事 |
|---|---|---|
| 内部研发 | 从 WorkPackage/Attempt/Accept 取证据 | 不得因为「有内部运行」就提高证据等级 |
| 外部导入 | 从来源修订、制品摘要、人工补证取证据 | 不得因为「没有内部运行」就降级或跳过核对 |

「同一结论」的判定方式：同一组 `CoverageInput` 字段值 → 同一 `CoverageVerdict`。
这条可以用纯函数直接测（I-72），不需要两边端到端跑完才敢说成立。

### D2 来源身份是五元组，不是 URL

身份 = Company + Project + 连接实例 + 来源对象类型 + 提供方稳定对象 ID。
只有 URL 或纯文本时**进候选**，由负责人确认后才成为身份。
代价：导入路径比「存个链接」麻烦得多；收益：跨项目同名 PR 不会互相覆盖。

两条去重语义分开：幂等键管「同一次命令重放」，来源身份管「不同命令导入同一对象」。
把两者合成一个键，会出现「重放被当成新导入」或「不同对象被当成重放」。

### D3 跟踪项不依赖内部 Attempt 外键

跟踪项必须能独立成立。实施前先盘查真实 schema/DAO/查询/UI 是否强制内部外键：

- 若允许独立成立 → 复用已有关系；
- 若受限 → 按 §33.7 做增量迁移或显式关联表，并用**真实旧库**验证：旧运行仍可追溯、
  外部记录可无运行、重复导入无重复回执。

禁止路径：为了满足外键而创建假 Attempt。那是在权威记录里写假数据，比缺功能严重。

### D4 草稿保存与外发是两条路径

草稿只写「目标 / 收件范围 / 证据修订 / 引用 / 缺口 / 文本修订」，保存动作**不产生任何出站副作用**。
发送要在两个生效点各查一次：命令受理时、出站工作者真正发时。两次之间证据或授权变化 →
旧草稿进待重审并提示差异，不允许「已入队就照发」。

发送结果与渠道送达分开记录：提交成功只表示意图已登记；回包丢失先对账，保持发送状态未知，
不推断为失败也不重发。

### D5 端点先冻结再实现

§11 的 6 个操作先带 `contract` 进路由表并重生成 `verification/openapi.json`，
路径冻结后才写 handler。理由：路径改名对客户端的破坏远大于实现顺序调整；
而冻结的成本是一条命令（ADR-0004 的黄金比对）。

冻结时定下的两点（实施后补记）：

- **产品接口单独一张表**（`delivery_api::routes`，前缀 `/api/v1`），不并入控制面
  `ROUTES`。分发层按前缀分流。理由是鉴权语义不同：控制面凭据是本机会话密钥，
  §11 要按 §34.12 的主体与目标判定；混在一张表里，文档会把两者写成同一种鉴权。
- **`x-bod-status`**：每条操作声明 `served` 或 `planned`（`Contract.status`，无默认值）。
  计划中操作一律 `planned` + 声明 501，handler 只有 `not_implemented` 一个；
  测试断言「planned 必须声明 501、served 不得声明 501」，实现落地时必须改状态。

当前鉴权只到会话级（`Auth::Bearer(BOD_API_DENIED)`），§34.12 未实现——
这一点写在每条路由的 `note` 里，也写进文档的 operation description。

## Risks / Trade-offs

| 风险 | 处理 |
|---|---|
| 旧库强制内部外键，迁移面比预期大 | 先盘点再动；必要时拆成两个切片（读路径先通、写路径后迁） |
| `coverage_receipt` 是零引用模块，接线时才发现设计与真实数据不匹配 | 首切片就用真实数据喂 `judge_coverage`，不先写「演示用」聚合 |
| 渠道无幂等能力时重复发送 | 标待核实 + 人工对账，不假装有 exactly-once |
| 6 个端点一次进表，开放文档的组件命名成为约束 | 组件名用聚合名（`DeliveryTracking`/`DeliveryDraft`），不用端点名 |

## Migration Plan

### 2.1 盘点结果（只读，2026-11-21；不改代码）

盘查范围：15 个迁移（50 张表）、`store.rs` 的 171 个查询函数、`apps/*` 全部前端源码。
结论先说：**没有任何外键指向 `attempts`/`runs`**，所以 D3 不需要靠迁移来「解绑」；
真正的阻断点只有一处——接受记录表把内部工作包修订写成了 `NOT NULL`。

| 检查项 | 实测 | 对 D3 的影响 |
|---|---|---|
| 外键约束 | 全库仅 1 条：`wake_receipts` → `work_queue(company_id, queue_key)`；**无一条指向 `attempts`/`runs`** | 跟踪项可独立成立，无需放宽或删除约束 |
| 强制内部身份的列 | `attempts.run_uid`、`invocations.run_id/attempt_id/binding_id/fencing_token`、`handoff_snapshots.source_attempt_id`、`takeover_intents.from_attempt_id` 均 `NOT NULL` | 这四张表只能装内部运行；外部导入不得借用（借了就得编 ID） |
| 接受记录 | `acceptances.work_package_revision` / `input_baseline` / `artifact_revision` 均 `NOT NULL`；`work_package_states` 主键为 `(issue_uid, revision_id)` | **阻断点**：外部交付没有内部工作包，写入必须编一个 WPR——正是 D3 禁止的路径。因此外部回执不复用 `acceptances`，另表承载 |
| 制品 | `artifacts.project_id NOT NULL`，无 attempt 列；`artifact_revisions.produced_by` 是自由文本 | 可复用：外部制品按项目归属登记，`produced_by` 记来源 |
| 验证记录 | `verification_records.revision_id NOT NULL`，指向制品修订 | 可复用：外部证据挂在制品修订上 |
| 查询/JOIN | `list_project_invocations`、`list_run_invocation_states` 经 `JOIN runs ON … JOIN issues` 过滤；`list_project_acceptances` 经 `JOIN artifacts` 取项目 | 前两条只对内部运行成立，外部跟踪项不得复用；后者不依赖 Attempt |
| 计数 | `count_attempts` 只数 `attempts` 表 | 外部记录本就不该进 `attempts`，现有计数语义天然正确，无需改 |
| UI | `apps/workbench` 只有 `connection.ts` / `main.ts` / `connection.test.ts`，无 attempt/run/acceptance 引用；`apps/desktop` 无相关查询 | 没有 UI 在强制「必须来自内部运行」 |
| 发送意图 | `outbox_deliveries.status` 已含 `unknown`，主键 `(event_or_command_id, destination_id)`，以 `queue_key` 关联 `work_queue`（该表无外键） | 可复用登记发送意图；`work_queue` 是通用持久队列，不是内部运行，建一条队列不算造假 |

**结论**：走「增量迁移 + 显式关联表」，不新增外键、不改既有列。
新增迁移只放跟踪项/来源身份/目标关联/草稿四类表，内部引用列全部可空；
制品与证据复用 005 的三张表；发送意图复用 002 的 `outbox_deliveries`。
旧运行可追溯性不受影响：既有表和查询一行未改，回归靠现有 1280+ 用例兜住。

### 步骤

1. ~~盘点（只读）~~：见上（2.1）。
2. 路径冻结：6 个端点进路由表 + 重生成 `verification/openapi.json`（2.2）。
3. 读路径：跟踪项与覆盖结论（可查询、不写入）。
4. 写路径：导入、关联、补证入现有 Company 单写事务；落点为新增表，不复用 `acceptances`。
5. 草稿与发送：保存 → 重审 → 发送（复用 `outbox_deliveries`）→ 对账。

每步独立可回滚；第 2 步之后路径不能再改，实现顺序可以改。

## Open Questions

- 渠道适配的第一条真实通道选哪个（决定「送达」是否有幂等键）。
- 人工补证的证据等级如何与连接器映射冲突时并列展示（§11 要求保留两者并进待核实）。
