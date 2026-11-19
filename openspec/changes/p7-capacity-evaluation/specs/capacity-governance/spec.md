# Spec Delta: capacity-governance

## Purpose

定义基于真实瓶颈的容量毕业门与引擎决策记录，防止按事件数或单一恢复指标机械迁移存储/执行引擎。

## ADDED Requirements

### Requirement: 毕业信号

容量毕业 SHALL 依据实测 RTO 不达标、单写争用、索引/数据体积压力、实际多机写入或容灾需求分别判定；MUST NOT 只以日均事件数或「RTO 小于一分钟」单一指标推出需要分布式系统。

#### Scenario: 单一高事件数
- **WHEN** 某系统日均事件数达到一百万但实测 RTO、争用和体积均在声明范围内
- **THEN** 系统不触发引擎迁移决定，只记录观察值与复测条件

### Requirement: 瓶颈定位

每次评估 SHALL 分别测量恢复耗时、写锁等待与 Busy、表/索引体积、查询/重建成本、Agent 与执行槽位占用，并指出实际瓶颈；缺失观测 MUST 标 unknown，不把无告警视为健康。

#### Scenario: 写争用定位
- **WHEN** 事务 p99 升高但 Busy 计数与写等待时间均未上升
- **THEN** 评估不把瓶颈归因为单写争用，继续定位其他因素并标 unknown 区域

### Requirement: 候选方案对比

系统 SHALL 对比索引/批处理/存储优化、执行节点扩展、PostgreSQL 与外部持久执行服务，逐项记录迁移代价、运维负担、故障语义、兼容窗口与恢复证据；DAO 替换 MUST NOT 被当作语义兼容证明。

#### Scenario: 提议直接换 PostgreSQL
- **WHEN** 一个提案仅以当前 SQLite 写争用为由要求切换到 PostgreSQL
- **THEN** 评估要求列出索引/批处理/执行扩展的对比与迁移风险，否则不形成毕业决定

### Requirement: 决定记录

GraduationDecision SHALL 记录瓶颈、样本与环境、候选对比、选定方案、责任主体、回滚路径、未覆盖条件与复测触发点；决定 MUST 可审计，不因人员变化改写历史依据。

#### Scenario: 审计容量决定
- **WHEN** 评审查看一次决定记录
- **THEN** 可看到原始报告引用、方案对比、批准主体、未测项与何时重新评估

### Requirement: 变更门控

容量评估 SHALL NOT 在本 Change 内改变业务数据或运行时语义；任何存储/执行引擎变更 MUST 另建 Change，包含迁移 ID/checksum、单写次序与并发测试、备份恢复演练、回滚路径和可验证支持矩阵。

#### Scenario: 评估中直接迁移
- **WHEN** 有人试图在评估任务里直接切换数据库引擎
- **THEN** 宿主阻止该变更并要求独立 Change 与完整迁移/恢复证据

### Requirement: 支持矩阵一致

容量结论 SHALL NOT 自动扩大功能支持矩阵；未测平台、协议与能力保持 Planned/Experimental/Unsupported，新的性能数字 MUST 与对应形态和版本绑定发布。

#### Scenario: 性能达标扩大支持
- **WHEN** headless 吞吐达标后有人把远程节点标为 Supported
- **THEN** 系统拒绝该宣称，要求对应远程能力的 conformance 与真机/双机报告
