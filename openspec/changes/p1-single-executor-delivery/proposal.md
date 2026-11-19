# Proposal: P1 单执行者交付

## Why

P0 建立了可信核心，但还没有证明一个用户能在不配置 Team、WorkPackage 或 Run 的情况下，用已有 Agent 对一个真实仓库完成一次可接受的交付（§40.1 P1、§41.3）。没有确定工作区、上下文与澄清边界时，同一规格在不同会话会被不同解释，候选成果和验收证据也难以绑定实际提交。

## What Changes

- 为每次 Attempt 建立独立工作区与 git worktree 基线，限制写范围与共享资源冲突；终态后按保留策略回收，不删除用户仓库或未确认现场。
- 定义 ContextBundle 确定装配、指令优先级与会话隔离；外部网页、工具结果与反馈保留不可信属性，角色变化不能扩大权限。
- 增加 Issue 讨论、Question/Answer 与 NeedsInput 稳定状态；普通评论无副作用，只有显式有权限命令才转为 Decision/FeedbackProposal。
- 打通一个真实仓库的直派闭环：交办 → 工作区执行 → 本地验证 → 人工或预授权接受 → 备份/恢复；取消与 Unknown 如实展示。
- 工作台提供四个首页问题与任务页证据视图；关闭桌面不终止宿主，也不丢失工作。
- 非目标：第二执行者与跨执行者接续、负责人委任与切换、OpenSpec 影响图与多 Agent 合并队列、CI/制品/环境发布、团队 Catalog 与远程执行。

## Capabilities

### New Capabilities

- `workspace-execution`: 单次 Attempt 的隔离工作区、写范围、共享资源协调与回收语义。
- `context-assembly`: ContextBundle 的确定装配、指令优先级、会话复用与来源边界。
- `clarity-dialogue`: Issue 讨论、Question/Answer、NeedsInput 与评论到决策的显式转换。
- `direct-delivery`: 无 Team/Run 配置的真实仓库直派闭环、本地验证、接受、备份与工作台可见性。

### Modified Capabilities

无。P0 已于 2026-11-19 归档，现行 `openspec/specs/` 含其 9 个 capability（57 条需求 / 66 个场景）；本 Change 的 4 个能力与现行规格均不同名，全部以 ADDED 新增，不改写 P0 现行规格与历史约定。

## Impact

- 代码：`crates/bod-app/src/coding/`（worktree/路径门禁/回收）、`crates/bod-app/src/context/`（装配与缓存）、`crates/bod-app/src/work/`（讨论与直派）、`apps/workbench`（任务页与首页待办）。
- 数据：新增 workspaces、context_bundles、questions/answers、local_verification、backups 等记录；沿用 P0 的 events、outbox、reservations、artifacts。
- 验证：绑定 I-10、I-17、I-26、I-36、I-55、G-11、G-15、G-16、G-29、G-51 等（§39.13—39.14）；初始为 planned。
- 依赖：P0 的 identity-access、coordination-loop、effect-gate、budget-admission、artifact-acceptance 与 execution-adapter 必须可用；不新增常驻服务或外部 MQ。
