# Spec Delta: conformance-toolkit

## Purpose

定义公开符合性工具的分项报告与准入边界，使第二运行时和接续能力只基于可核对证据，而不是单个绿色徽章或自报声明。

## ADDED Requirements

### Requirement: 分项报告

`bod conformance` SHALL 复用 schema、fixtures 与状态映射规则，按适配器版本、协议版本、平台、能力与日期输出分项报告；MUST NOT 用单个通过标记掩盖未支持或 Unknown 项。

#### Scenario: 生成报告
- **WHEN** 对一个适配器运行符合性工具
- **THEN** 报告逐项列出能力、用例结果、版本、平台与日期，未测项明确标记 Unknown/NotApplicable

### Requirement: 必需用例集合

必需用例 SHALL 覆盖范围身份、重复 start、参数冲突、丢回包、迟到观察、取消未确认、版本不匹配、敏感字段过滤、外部 completed 与接受分离。身份、授权或消息完整性用例失败时 MUST 禁用该适配器执行，不能通过声明 degraded 豁免。

#### Scenario: 身份用例失败
- **WHEN** 一个适配器在范围身份用例中失败
- **THEN** 工具给出阻断结论，宿主拒绝启用该适配器执行，即使其他用例通过

### Requirement: 可选能力只在声明支持时验证

cancel/resume/reconcile/quota_windows/ACK 等可选能力 SHALL 仅在适配器声明支持时验证；不支持恢复的适配器可安装，但 MUST 禁止派发需要恢复保证的任务，并在调度准入与界面显示同一限制。

#### Scenario: 不支持恢复的适配器
- **WHEN** 调度器遇到一个需要会话恢复保证的工作包而候选适配器不支持 recovery
- **THEN** 该候选被过滤，界面显示缺失能力原因，不产生 Binding

### Requirement: 双运行时接续证据

公开接续支持 SHALL 至少有两个已测运行配置在相同任务样本上完成 L1/L2（及适用时的 L3）接续，报告记录环境、版本、样本、失败路径与人工耗时。不能凭同一运行时恢复会话宣称跨运行时接续。

#### Scenario: 申报接续支持
- **WHEN** 团队试图在工作台声明某组合支持跨执行者接续
- **THEN** 系统要求两个已测运行配置的报告与样本；缺任一项时保持 Planned/Unverified

### Requirement: 发布验收项

适配器发布 SHALL 核验停止与重启意图、重放与失联、副作用与授权、账号与预算、验收证据；能力缺失 MUST 在探测结果、调度准入和用户界面一致体现，日志中不得出现凭据值。

#### Scenario: 凭据出现在日志
- **WHEN** 发布验收发现适配器日志包含凭据值
- **THEN** 该项不通过，适配器不得标 Supported，报告保留脱敏后的失败证据
