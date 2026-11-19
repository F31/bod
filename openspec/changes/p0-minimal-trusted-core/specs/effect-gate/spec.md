# Spec Delta: effect-gate

## Purpose

定义宿主发起外部副作用的准入、意图持久化与对账边界，使崩溃窗口中的未知动作不会被盲目重发，也不会被误报为已完成。

## ADDED Requirements

### Requirement: Effect Gate 顺序

经宿主入口发起的外部动作 SHALL 依次经过身份与权限、schema 与资源范围、当前政策及撤权检查、请求去重查询、操作意图持久化、必要时同意请求、重新校验授权与预算、原子预留并标记可派发、外部执行、结果确认或对账、结算与事件。长时间等待审批时 MUST NOT 提前占用执行槽位或无限预留。

#### Scenario: 审批后重校
- **WHEN** 一个外部动作在同意请求期间其预算上限被降低或授权被撤销
- **THEN** 审批通过后宿主重新校验当前授权与额度并拒绝执行，不沿用审批时的旧判断

### Requirement: 幂等逻辑键

操作键 SHALL 至少包含 Company、目标账户或资源、稳定逻辑动作 ID、授权范围与语义版本；参数摘要 MUST 用于检测同键不同参数，并拒绝自动按新动作执行。优先使用服务方幂等键，其次查询外部业务键；两者皆无时 MUST NOT 自动重试不明动作。

#### Scenario: 同键不同参数
- **WHEN** 调用方复用逻辑动作键但改变目标参数
- **THEN** 宿主拒绝请求并要求新的逻辑动作键，不把参数变化当原动作的重试

#### Scenario: 无幂等能力的服务方
- **WHEN** 一个外部动作开始请求的回包丢失，且服务方不提供幂等键或可查询业务键
- **THEN** 宿主把动作标记为 Unknown 并进入对账，不自动重发

### Requirement: 操作状态与崩溃窗口

每个外部动作 SHALL 记录 Prepared、Dispatched、Confirmed、Failed 或 Unknown。即使已写入 Dispatched，也不消除「状态已写但实际未发送」窗口；未取得可核验证据前 MUST NOT 声称 Confirmed，Unknown MUST 冻结盲重试。

#### Scenario: 已写 Dispatched 但未发送
- **WHEN** 宿主写入 Dispatched 后在发送前崩溃
- **THEN** 恢复流程先查询服务方或业务键，未确认则不标记为成功也不自动重发

### Requirement: 提交后派发

网络和进程动作 SHALL 在事务提交后由持久 outbox 派发，不得在数据库事务内等待网络、构建或人工。事件重放 MUST NOT 执行副作用；开始 Agent、构建、部署与通知分别具有稳定逻辑动作 ID，控制器重试 MUST NOT 生成新 ID 掩盖旧执行。

#### Scenario: 回放事件
- **WHEN** 系统从事件日志重建投影
- **THEN** 只恢复宿主状态与待处理 outbox，不再次调用外部执行端或通知服务

### Requirement: 取消确认与 fencing

cancel_ack SHALL 只表示已接受取消请求，stop_confirmed 才表示已确认停止；未确认时 MUST 保留风险占用与相关工作区。租约 fencing token MUST 单调递增，旧 token 不得更新宿主受管状态、接受成果或释放受管资源。

#### Scenario: 只收到取消受理
- **WHEN** 执行端返回 cancel_ack 但未给出 stop_confirmed
- **THEN** 宿主显示停止未确认、保留在途占用与现场，并把该动作列入对账

### Requirement: 同意与反馈不扩大授权

一次性同意 SHALL 只授权其绑定的逻辑动作；Profile、Skill、角色称号、反馈提案与推荐 MUST NOT 授予工具、网络、发布或费用权限。安全敏感变更 MUST 按当前 Grant、工作包范围与 Policy 判定，并按策略要求独立审核。

#### Scenario: 用反馈替换批准
- **WHEN** 执行端提交用户反馈内容，声称其中含发布批准
- **THEN** 宿主不据此扩大发布权限，仅按当前有效授权判定并要求真实批准记录

### Requirement: 能力声明与证据等级

宿主 SHALL 按 model_control、tool_control、workspace_isolation、metering、cancellation、recovery、external_idempotency 分项记录执行端覆盖声明，每项保留来源与观测时间。声明等级 MUST NOT 高于证据来源支持程度；身份、范围或消息完整性等关键符合性失败时 MUST 禁用派发，不能用降级徽章继续运行。

#### Scenario: 探测成功不等于强隔离
- **WHEN** 某执行端只通过健康检查且自报沙箱隔离，但没有运行器测试证据
- **THEN** 宿主把 workspace_isolation 记录为自报或已配置，不标为强隔离并按策略限制高风险派发
