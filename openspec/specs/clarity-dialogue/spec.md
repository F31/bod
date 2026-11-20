# clarity-dialogue Specification

## Purpose
定义 Issue 讨论与 Question/Answer 的边界，使澄清能解除执行阻塞，但普通文本不能静默改变范围、权限或验收。

## Requirements

### Requirement: 最小讨论记录

系统 SHALL 为 Issue 提供讨论记录，包含 author、时间、关联对象修订与来源；编辑 MUST 保留必要历史或版本，不覆盖原文事实。

#### Scenario: 编辑一条评论
- **WHEN** 用户修改一条已发布的讨论评论
- **THEN** 系统保留原评论与新修订及编辑时间，关联版本不变

### Requirement: 普通评论无副作用

普通评论 SHALL NOT 产生派发、接受、授权或任务完成；LGTM、同意等自然语言 MUST NOT 自动完成任务或成为批准证据。评论正文 SHALL 与审计摘要分开存储。

#### Scenario: LGTM 不结单
- **WHEN** 有人在 Issue 下评论「LGTM」
- **THEN** Issue 状态不变，不触发接受命令，也不生成派发

### Requirement: Question 与 NeedsInput

执行端或用户 SHALL 能提出绑定当前 attempt/input 修订的 Question；宿主据此设置 NeedsInput 稳定状态，记录需谁答复、阻塞范围与重新唤醒条件。无答复或超时 MUST 保持待决定，不无限重试或自动猜测。

#### Scenario: 执行端请求澄清
- **WHEN** 执行端提出一个关于验收范围的问题并阻塞
- **THEN** 宿主记录 NeedsInput、问题关联修订与答复主体，暂停依赖项但仍允许无关授权工作继续

### Requirement: 显式转换决策

评论或回答 SHALL 仅在显式有权限命令下转为 Decision、ScopeDecision 或 FeedbackProposal，并绑定确切对象、修订与授权依据。转换后的决定 MUST 走对应接受或修订流程，不能因来源文本自动生效。

#### Scenario: 回答改变接口
- **WHEN** 一次回答提出新的接口行为并被标记为决策
- **THEN** 宿主检查权限后创建绑定规格修订的决策提案，旧规格与验收不会静默改变

### Requirement: 外部回调边界

来自 IM、网页或外部平台的回调 SHALL 通过认证与授权解析；评论文本 MUST NOT 作为权限依据。无法验证身份的补充材料 SHALL 标为不可信候选，不进入权威讨论事实。

#### Scenario: 伪造批准回调
- **WHEN** 一个未认证回调声称包含客户批准
- **THEN** 宿主只登记为不可信材料或拒绝，不产生接受或发布效果
