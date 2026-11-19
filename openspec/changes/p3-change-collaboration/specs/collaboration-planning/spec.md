# Spec Delta: collaboration-planning

## Purpose

定义默认扁平的协作计划提案与执行图准入，使分工按依赖与权限确定，顾问不能自行派发，共享 Agent 不串会话。

## ADDED Requirements

### Requirement: 默认扁平协作

系统 SHALL 默认按工作依赖、接口和成果交接组织协作，不要求组织层级或 Team 绑定；普通答疑可按职责路由，改变需求、权限或验收的答案 MUST 走各自修订或授权流程。

#### Scenario: 普通答疑
- **WHEN** 一个执行者向另一成员提出接口用法问题
- **THEN** 宿主按职责路由答疑，不产生派发、权限变化或验收修改

### Requirement: 计划提案与安排权限

PlanProposal SHALL 固定输入规格、任务映射、工作包修订、依赖边、成果要求与风险。未委任协调顾问只能提交候选；已委任负责人可在 Grant 内提交接受/安排命令，宿主校验后再形成 PlanRevision。

#### Scenario: 顾问试图派发
- **WHEN** 一个未委任顾问提交包含实际派发命令的提案
- **THEN** 宿主只保留候选部分并拒绝派发命令，不产生 Assignment 或 Binding

### Requirement: 执行图准入

宿主 SHALL 校验图无非法循环、需求/场景映射、共享资源冲突与权限后才形成 PlanRevision；需要业务判断的覆盖 MUST NOT 只凭 schema 宣称正确。Ready 队列 SHALL 来自已接受图，不直接使用 Agent 自然语言步骤。

#### Scenario: 循环依赖
- **WHEN** 计划中两个工作包互相声明依赖
- **THEN** 宿主拒绝该图并指出循环，不创建 Ready 项

### Requirement: 计划变更边界

计划修改 SHALL NOT 覆盖在途 Run；影响已有执行时 MUST 采用显式暂停、修订迁移或新 Run。新规格、新输入或新验收条件形成工作包修订，而不是修改历史约定。

#### Scenario: 修改在途计划
- **WHEN** 负责人想在 Run 运行中调整依赖图
- **THEN** 系统要求在途 Run 继续原计划，新安排通过新 Run 或显式迁移生效

### Requirement: 共享 Agent 会话隔离

调度资源键 SHALL 包含 Company、Project/作用域、目标身份、工作类别与必要的执行/委任身份；共享 Agent 的全局并发占用另行约束，不同项目 MUST NOT 合并到同一会话或复用敏感历史。

#### Scenario: 同一 Agent 两个项目
- **WHEN** 一个 Agent 同时被安排到项目 A 和 B 的工作包
- **THEN** 两个 Attempt 使用不同会话、工作区与 ContextBundle，并发上限分别校验

### Requirement: 协作建议可审计

协作方式选择、建议与计划修订 SHALL 记录依据、触发修订与风险；模型可提出语义影响与拆分建议，确定性宿主 MUST 检查权限与依赖，不能确认的关系标为未知。

#### Scenario: 不确定的协作建议
- **WHEN** 模型建议把两个模块并行但未给出依赖依据
- **THEN** 宿主把关系标为未知，不直接接受为执行图，要求负责人确认
