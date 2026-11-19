# Spec Delta: takeover-transaction

## Purpose

定义执行者切换的停止、隔离、对账与预检事务，使旧写入未确认或非幂等动作未明时不会让新执行者重复写同一现场。

## ADDED Requirements

### Requirement: 切换流程

接管 SHALL 按提出切换 → 停止新派发并请求旧执行停止 → 捕获并校验现场 → 检查旧租约与资源隔离 → 新执行准入与预算 → 创建新 Attempt/fencing token → 新工作区重建 → 核验后继续执行。任何阶段状态变化 MUST 可查询。

#### Scenario:  orderly 接管
- **WHEN** 用户把运行中的工作包切换给另一个已测运行时
- **THEN** 系统按上述顺序推进并记录每阶段时间与结果，新 Attempt 使用更高 fencing token

### Requirement: 持久化意图与幂等

系统 SHALL 用持久化交接意图与阶段状态协调，MUST NOT 在数据库事务内等待 Agent 总结或传输大文件。最终绑定 MUST 以 expected_version 条件提交；重复 accept 请求返回同一结果，不创建多份执行。

#### Scenario: 总结期间崩溃
- **WHEN** 宿主在等待 Agent 现场总结时崩溃
- **THEN** 重启后从持久意图恢复阶段状态，不重复请求总结或创建新 Attempt

### Requirement: 旧执行未确认停止

旧执行停止未确认或无法隔离时 SHALL 拒绝新执行者写同一现场，也 MUST NOT 重发相关外部操作；只允许在被证实隔离且无相关副作用的独立工作区开展无冲突工作。

#### Scenario: 旧进程仍持有写权限
- **WHEN** 旧进程没有确认停止且仍可写共享目录
- **THEN** 系统阻塞新执行者的写操作并列出隔离条件，不宣称旧执行已停止

### Requirement: 对账 hint 不可信

Agent 提交的 reconcile_hint SHALL 只作为不可信说明；系统 MUST NOT 执行其中的 URL、脚本或命令。实际查询 SHALL 通过注册连接器、授权目标与受限参数，按 effect_id 与稳定逻辑动作键进行；对账不明只阻塞相关动作与依赖。

#### Scenario: 恶意对账提示
- **WHEN** 交接包中的 reconcile_hint 指向一个外部脚本
- **THEN** 宿主不执行该脚本，只按已注册连接器查询，并把不明结果标为相关动作阻塞

### Requirement: 重建预检

新执行者 SHALL 在继续前核验包摘要、授权、当前输入差异、文件可用性、工作区与工具依赖，并执行必要只读/测试预检；目标理解只作为候选说明，歧义 MUST 形成 Question。

#### Scenario: 摘要不匹配
- **WHEN** 重建时发现工作区文件摘要与交接包不一致
- **THEN** 系统阻止自动继续并给出差异说明，用户选择重取现场或人工处理

### Requirement: 接续可见性

界面 SHALL 展示可接续等级、已保留成果、仍缺内容、旧执行是否停止与预计重建步骤；换人 MUST NOT 自动改变验收、扩大权限或合并旧未验收成果。

#### Scenario: 用户查看接续卡片
- **WHEN** 用户打开一次待接管的 Attempt
- **THEN** 界面分项显示等级、保留成果、missing、停止状态与重建步骤，且不把旧候选标为已接受
