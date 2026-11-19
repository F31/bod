# recovery-cleanup Specification

## Purpose
定义最小诊断、备份恢复与清理阻塞行为，使宿主在崩溃、磁盘异常或未确认外部动作后给出可核对事实与下一步，而不是静默清空状态。

## Requirements

### Requirement: 最小 doctor

doctor SHALL 检查数据库可写、队列深度与最老未处理时间、租约过期、outbox 积压、Unknown 事件与备份新鲜度，并输出带原因与可操作下一步的结果。磁盘满或关键记录写入失败时 MUST 停止新派发并保留未确认接收状态，不得继续外部动作。

#### Scenario: 磁盘满
- **WHEN** 宿主检测到数据目录磁盘已满
- **THEN** doctor 报告该故障，宿主停止新派发、保留在途与未确认状态并告警

### Requirement: 备份内容

备份 SHALL 使用 SQLite 一致性备份 API 或已验证快照，包含数据库及其 WAL、引用的 CAS 对象、配置与适配器版本，以及独立密钥恢复说明。MUST NOT 只复制活动主文件而遗漏 WAL。

#### Scenario: 备份核验
- **WHEN** 用户执行一次备份并校验结果
- **THEN** 报告列出数据库、WAL、CAS 引用与版本信息，缺任一项时备份不标为可恢复

### Requirement: 恢复到新宿主

恢复 SHALL 先验证备份并独占宿主，再恢复当前表、投影与队列，随后核验进程、会话/凭据、委任状态与在途 Effect；Unknown MUST 先对账，之后才允许开放新派发。缺历史签名时 MUST 标为无节点签名，不补造证据。

#### Scenario: 在途 Effect 的恢复
- **WHEN** 备份恢复到新宿主后仍有一条 Dispatched 的外部动作
- **THEN** 系统保留风险占用，先查询外部状态或进入对账案件，不自动按新任务重新派发

### Requirement: 对账案件

不明外部动作或用量 SHALL 建立 ReconciliationCase，记录 invocation、逻辑动作键、影响范围、风险占用、已知证据与人工裁决入口。超时 MUST NOT 被当作外部失败；未确认费用与占用 MUST NOT 被清空或按普通 TTL 提前回收。

#### Scenario: 外部结果不明
- **WHEN** 一个非幂等动作的查询持续超时
- **THEN** 系统创建对账案件并保留风险占用，界面显示 ExternalOutcomeUnknown 与处理人，不标为失败也不重试

### Requirement: 删除请求与清理阻塞

清理 SHALL 先标记删除请求并停止新派发，再按执行停止已确认、费用已结算或移交对账、产物已按策略固定、临时环境已处置逐项判定。任一项不明 MUST 显示 CleanupBlocked 及原因；Abandon SHALL 记录尚存外部资源、风险、责任人与后续对账入口，不标为删除成功且无残留。

#### Scenario: 未确认停止时清理
- **WHEN** 用户清理一个取消尚未确认的 Run
- **THEN** 系统显示 CleanupBlocked 并列出未确认停止与在途占用，不允许静默清空记录

### Requirement: 外部共享资源边界

OwnerReference SHALL 只对宿主管理的依赖资源做受控级联；用户仓库、生产环境、外部共享资源与正式 Manifest 引用的成果 MUST NOT 因项目删除被自动销毁。清理函数 MUST 幂等并经过 Effect Gate。

#### Scenario: 删除项目保留仓库
- **WHEN** 用户删除一个关联外部 Git 仓库的 Project
- **THEN** 宿主只清理自身管理的记录与临时环境，外部仓库和已交付成果保留，并说明哪些引用仍需人工处理

### Requirement: 迁移与恢复测试

每项迁移 SHALL 登记不可变 migration_id、checksum、依赖、适用的 schema/software 范围、风险与兼容窗口、数据转换说明及备份恢复方式。已发布脚本 MUST NOT 原地改写；破坏性 DDL 默认阻断，确需执行时必须显式风险标记、授权维护窗口与旧库升级/恢复测试。

#### Scenario: 已发布迁移被发现错误
- **WHEN** 一个已发布的迁移脚本被发现有缺陷
- **THEN** 系统禁止原地修改，要求新增迁移并给出旧库升级与恢复测试证据
