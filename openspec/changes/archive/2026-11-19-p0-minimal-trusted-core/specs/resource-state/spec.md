# Spec Delta: resource-state

## Purpose

定义资源外壳与期望/实际分离，保证状态只能由有权限的写入者通过领域命令改变，并给出可解释的终态、条件与删除语义。

## ADDED Requirements

### Requirement: 资源身份与条件更新

需要并发与状态投影的资源 SHALL 暴露稳定 uid、服务端生成的 resourceVersion、generation 与 observedGeneration；只有期望配置变化才递增 generation，状态观察变化 MUST NOT 递增。状态改变命令 MUST 携带 expected_resource_version，版本不匹配时拒绝并返回当前版本。

#### Scenario: 并发更新冲突
- **WHEN** 两个命令以同一个 expected_resource_version 更新一个 Run 的控制字段
- **THEN** 只有一个提交成功，另一个被拒绝并返回冲突与当前版本；任一命令不覆盖另一命令的写入

#### Scenario: 观察不改变代数
- **WHEN** 执行端上报一次进度观察，宿主只更新 status 与 observedGeneration
- **THEN** generation 保持不变，状态变化不被误认成期望配置变更

### Requirement: 期望与执行约定分离

Run 创建时 SHALL 固定 plan、工作包、规格修订与验收修订；允许改变 desiredState、受授权的资源限制与注释等控制字段，MUST NOT 直接修改运行中的输入或接受标准。需要新计划时 MUST 新建 PlanRevision 与 Run，或执行显式可审计的修订迁移。

#### Scenario: 运行中改验收标准
- **WHEN** 用户试图在 Run 运行中降低验收条件
- **THEN** 宿主拒绝该修改，要求新建带确定修订的 Run 或走显式迁移，原 Run 输入与接受标准保持不可变

### Requirement: 状态写入所有权

系统 SHALL 按数据类别限定主写入者：领域命令服务写工作包与计划，调度服务写 Binding 与租约，Run 控制器写 Attempt 状态，验证器写验证记录，接受服务写接受决策，可信预算服务写账本与预留。控制器 MUST NOT 通过通用 patch 修改不属于自己的状态；外部状态投影 MUST 带 observed_at、source_version 与 sync_status，并允许 Unknown、Stale、Unavailable。

#### Scenario: 执行端自报成功
- **WHEN** 执行端直接提交一个声称 Attempt 已成功的状态更新
- **THEN** 宿主只登记为观察或候选结果，由 Run 控制器按验证来源决定状态，进程退出码不成为接受事实

### Requirement: 正确停止状态

系统 SHALL 把 AwaitingApproval、NoEligibleAgent、InsufficientBudget、DependencyNotAccepted、ExternalOutcomeUnknown 作为合法稳定状态，记录原因、需谁处理与重新激活条件。一次性 Run 进入 Succeeded 或 Cancelled 后 MUST NOT 因残留期望字段自动重新执行；Cancelled 不得在外部停止未确认时声称全部停止。

#### Scenario: 等待审批不热重试
- **WHEN** 一个工作包因预算授权缺失进入 AwaitingApproval
- **THEN** 系统停止派发、记录批准主体与恢复条件，不产生周期性重试或额外预留

#### Scenario: 成功后不重跑
- **WHEN** 一次性 Run 已 Succeeded 且 desiredState 仍为 Running
- **THEN** 系统只查询/归档或创建后继 Run，不重新启动同一 Run

### Requirement: 条件与 phase 各司其职

资源 status SHALL 同时表达 phase 与 conditions；condition 的 reason 使用稳定枚举，message 仅面向人读，权限与调度逻辑 MUST NOT 解析 message 文本。observedGeneration 只表示协调已处理该代目标，不代表验收完成。

#### Scenario: 用 reason 判定阻塞
- **WHEN** 控制器判断一个资源是否可继续推进
- **THEN** 它依据稳定 reason 与 generation 判定，而不比较人读 message；界面显示 observedGeneration 时不标为已验收

### Requirement: 删除请求与清理边界

对象 SHALL 先标记 deletionRequestedAt 并停止新派发，再处理在途执行、费用、成果与临时环境。任一清理条件不明时 MUST 显示 CleanupBlocked 与原因，不得强行清空记录；OwnerReference 只对宿主管理的依赖资源受控级联，用户仓库、生产环境与外部共享资源 MUST NOT 自动销毁。

#### Scenario: 在途执行时请求删除
- **WHEN** 用户对仍有在途执行与未结算占用的 Project 请求删除
- **THEN** 系统标记删除请求并停止新派发，显示 CleanupBlocked 及具体未决项，保留外部仓库与生产资源
