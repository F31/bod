# Spec Delta: budget-admission

## Purpose

定义预算与资源准入的可信事务边界，使并发派发不超分、在途占用互斥、超额费用如实入账并给出可解释的拒绝原因。

## ADDED Requirements

### Requirement: 分层准入事务

Company、Project、Agent、Run、WorkPackage、执行目标槽位、session/workspace 与 Reservation/Binding/派发意图 SHALL 在同一个 UnitOfWork 内按固定次序复核；同类多对象按稳定 ID 排序。任一步失败 MUST 整体回滚，不允许部分占用；提交后才可执行外部动作。

#### Scenario: 并发超分被阻止
- **WHEN** 两个 Issue 同时申请同一 Company 的最后一份预算额度
- **THEN** 只有一个事务通过准入并创建预留，另一个因 InsufficientBudget 整体回滚且不产生 Binding 或派发意图

### Requirement: 确认、预留与风险互斥

可用额度 SHALL 等于限额减已确认消费 C、有效预留 R 与待对账风险占用 U。同一未结算金额 MUST NOT 同时计入 R 与 U；取消或过期但无法确认的剩余占用 MUST 在同一事务内从 R 转 U，一减一增，不额外重复扣减。

#### Scenario: 取消后转风险占用
- **WHEN** 一个已发出的动作被取消但外部是否发生作用未知
- **THEN** 系统在事务内把对应金额从 R 转为 U，可用额度保持扣减，后续不把该金额当空闲额度再次分配

### Requirement: 超额如实入账

实际费用 SHALL 通过不可变分录与有来源的调整分录记账，外部 usage_key 唯一。实际超过估计时 MUST 如实追加超额并记录原因，同时阻止后续操作；MUST NOT 截断 actual 以保持 settle 不超过 reserve 的假平衡。

#### Scenario: 最终账单高于预留
- **WHEN** 服务方迟到上报的费用高于派发时的预留上界
- **THEN** 系统追加实际费用与超额调整分录，标记该 Project 预算超额并拒绝新的派发

### Requirement: 多维约束与计量单位

金额、时长、token、调用次数、并发、磁盘与订阅配额 SHALL 同时作为可用约束，任一达到限制即阻止对应新操作；MUST NOT 用单一 budget_basis 屏蔽其他限制。金额 MUST 使用明确币种与整数最小计价单位或经验证定点数，不同模型的 token MUST NOT 直接相加为费用。

#### Scenario: token 不是统一货币
- **WHEN** 两个不同模型分别报告 token 用量
- **THEN** 系统按各自口径记录并分开展示，不把它们相加得出一个现金金额或据此绕过金额上限

### Requirement: Busy 与稳定 request_id

写事务 SHALL 使用有界等待与短事务，冲突在请求总 deadline 内整体重读并有限重试，而不是只重放最后一条 SQL。超时 MUST 返回可恢复 Busy 并保留稳定 request_id；默认同步与持久性配置 MUST NOT 为降低尾延迟被静默改写。

#### Scenario: 写冲突后整体重试
- **WHEN** 一次准入事务遇到数据库 Busy
- **THEN** 系统在 deadline 内重新读取全部相关状态再重试整个事务，超时返回带 request_id 的可恢复错误

### Requirement: 默认无隐式付费授权

没有用户显式授权的付费额度时 SHALL NOT 自动创建默认可消费额度。免费、离线预览与观察模式 MUST 明确标识；自动接受与代理付费 MUST 有预先授权、范围与时效，并计入对应项目。

#### Scenario: 首次启动默认配置
- **WHEN** 用户在没有任何付费配置的情况下首次使用宿主
- **THEN** 系统不自动授予消费额度，任何付费探测或自动动作都要求显式授权

### Requirement: 预算解释与拒绝原因

系统 SHALL 按项目、Agent 与工作包展示已结算、有效预留、未知风险占用、可用额度、计量来源与覆盖缺口，并说明拒绝原因及允许的下一步。InsufficientBudget 等稳定原因 MUST 记录触发资源与重新唤醒条件，不同单位分开显示且不直接相加。

#### Scenario: 解释拒绝
- **WHEN** 用户查看一个因预算被阻止的工作包
- **THEN** 界面列出 C/R/U 数值与来源、缺失覆盖、批准主体和解除阻塞的下一步，不把未知占用显示为可用余额
