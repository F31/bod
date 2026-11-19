# Proposal: P4 实际运行交付

## Why

P3 能让工作包在真实提交组合上被接受，但用户最终关心的是实际用上的版本：哪份制品部署到了哪个环境、哪些检查通过、客户是否已覆盖、回滚后回执是否纠正。没有运行验收、不可变 Manifest 与可核对的覆盖回执，就会出现「测试通过但现场未更新」「答复已修复但客户仍在旧版本」的错配（§21.7、§28.7、§29.7、§40.8）。

## What Changes

- 定义 VerificationPlan 与 BuildRecord：把 Scenario/Criterion 映射到可执行检查、工具、数据、环境与预期结果；同一候选一次构建、按 digest 晋升，证据绑定实际制品与环境。
- 定义阶段基线 Manifest 与不可变 Publication：冻结一组具体修订，支持 internal/customer/audit 视图与字段级最小披露；签名、身份与业务正确分开。
- 定义 ReleasePlan/EnvironmentRef 与发布回滚：审批绑定实际 digest、环境、参数与有效期；部署状态与真实运行分开，回滚只恢复声明可回退资源。
- 定义逐客户交付证明与回执：按项目/环境/部署单元/制品摘要核对覆盖，缺证据标待核实；回滚产生新事实并重发更正，成本与验收结算分列。
- 非目标：Team Catalog 与六阶段权限开放（P5）、SDK/远程节点/移动客户端（P6）、按真实负载决定引擎迁移（P7）。

## Capabilities

### New Capabilities

- `runtime-verification`: 验证计划、构建记录、质量门禁与实际运行验收的证据绑定。
- `stage-manifest`: 阶段不可变清单、发布记录、证据包视图与签名/身份边界。
- `release-governance`: 环境引用、ReleasePlan、审批/回滚、预算解释与维护触发。
- `coverage-receipt`: 逐客户覆盖判定、证明页、确认/异议/复开与更正通知。

### Modified Capabilities

无。截至 P0 归档（2026-11-19），现行 `openspec/specs/` 仅有 P0 的 9 个 capability；本 Change 的能力与之均不同名，全部以 ADDED 新增，不改写既有规格与历史约定。

## Impact

- 代码：`crates/bod-app/src/verification/`、`crates/bod-app/src/release/`、`crates/bod-app/src/connectors/`（部署与通知）、`crates/bod-app/src/query/`（证明页投影）。
- 数据：新增 verification_plans、build_records、manifests、publications、environment_refs、release_plans、delivery_targets、feedback_receipts、coverage_observations。
- 验证：绑定 I-07、I-08、I-45、I-51—52、G-9、G-24、G-32—G-33（§39.10、§39.13—39.14）；初始 planned。
- 依赖：P1 本地验证、P2 历史证据、P3 集成接受必须可追溯；不新建 CI 引擎、浏览器引擎或外部 MQ。
