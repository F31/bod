# Proposal: P3 变更与有效协作

## Why

P1/P2 解决单个执行者与接续，但真实小团队会遇到共享接口变化：前端、后端、SDK 与测试各自修改同一规格，靠人工查依赖容易漏项或无效重验（§1.4、§21.11—21.12）。没有共享规格基线、任务稳定映射与受控合并，就会出现 Agent 各维护一套清单、共享会话串扰或「各分支绿灯但集成失败」。

## What Changes

- 建立 OpenSpec 集成：Change 目录映射到项目 Change，tasks.md 条目映射到稳定 TaskDefinitionRef，按确定修订派生工作包，勾选只作进度投影，归档由单一入口执行。
- 定义显式影响图：规格、接口、源码、测试、构建、环境与接受记录指向确定修订；区分确定、推测与未知关系，上游变化生成 ImpactAssessment 与有限重验提案。
- 定义单/多 Agent 协作规则与 PlanProposal→PlanRevision 准入：默认扁平、按依赖分工、无强制管理层；共享 Agent 不跨项目或跨任务串会话。
- 定义单仓库多工作区与受控合并队列：独立 worktree/分支、共享资源协调、契约冲突门禁、合并前重建候选验证，禁止以各分支通过代替最终接受。
- 非目标：CI 制品/环境组合与阶段 Manifest 的完整发布链（P4）、Team Catalog 与六阶段权限（P5）、SDK/远程/移动（P6）、容量迁移（P7）。

## Capabilities

### New Capabilities

- `openspec-integration`: 规格修订、任务稳定映射、工作包派生、同步与单一归档入口。
- `impact-analysis`: 依赖边分类、影响评估、选择性重验与有限维护提案。
- `multi-agent-workspace`: 多 worktree 协作、共享资源协调、合并队列与冲突门禁。
- `collaboration-planning`: 默认扁平的计划提案/修订、依赖图准入与共享 Agent 会话隔离。

### Modified Capabilities

无。截至 P0 归档（2026-11-19），现行 `openspec/specs/` 仅有 P0 的 9 个 capability；本 Change 的能力与之均不同名，全部以 ADDED 新增，不改写既有规格与历史约定。

## Impact

- 代码：`crates/bod-app/src/coding/`（OpenSpec 映射、worktree 合并队列）、`crates/bod-app/src/work/`（Plan 图与依赖）、`crates/bod-app/src/governance/`（委派/顾问提案边界）、`xtask`（影响索引与同步）。
- 数据：新增 spec_bindings、task_mappings、work_package_revisions、impact_assessments、merge_requests/merge_evidence、revalidation_plans；沿用 P1 工作区与 P2 检查点。
- 验证：绑定 I-10、I-13、I-20、I-53、I-54、G-7、G-31（§39.10、§39.13—39.14）；初始 planned。
- 依赖：P1 隔离工作区与 P2 接续必须可用；不引入全局 Agent 聊天网或通用工作流引擎。
