# execution-adapter Specification

## Purpose
定义宿主与执行端之间的最小生命周期契约与能力声明，使启动、查询和观察在回包丢失或乱序时仍可解释、可去重，且不把执行完成误当业务接受。

## Requirements

### Requirement: 必需操作与分项探测

适配器 SHALL 支持 describe、probe、start、inspect，并以流或轮询至少一种方式提供 observe。probe SHALL 分项返回 ready、not_ready、unsupported 或 unknown，每项带 reason、observed_at、expires_at、source 与 scope；凭据探测只检查授权状态，不回传密钥。

#### Scenario: 分项探测部分失败
- **WHEN** 执行端可执行程序就绪但模型凭据失效
- **THEN** probe 分别给出环境 ready 与凭据 not_ready 及原因，不返回笼统的整体成功

#### Scenario: 不自动付费探测
- **WHEN** 宿主执行后台健康检查
- **THEN** 不触发交互登录，也不调用付费推理探测

### Requirement: 能力分项声明

宿主 SHALL 分别记录 model_control、tool_control、workspace_isolation、metering、cancellation、recovery 与 external_idempotency；每项声明保留适配器自报、平台探测、测试结果或人工覆盖的来源与时间。未知能力 MUST 标 Unsupported 或 unknown，不得用单一 trust_tier 代替，也不得空称成功。

#### Scenario: 不支持取消的运行时
- **WHEN** 某执行端未声明 cancellation 能力
- **THEN** 界面显示取消请求不受支持，宿主不承诺停止确认或立即释放相关占用

### Requirement: Start 请求与持久去重

start SHALL 携带稳定 invocation_id、run_id、work_package_revision、attempt_id、binding_id、fencing_token 以及经授权解析的 agent、runtime、context、workspace、grant 与 reservation 引用。执行端 MUST 先持久化接收再确认；同 invocation_id 同参数返回原 execution_id，同 ID 不同参数 MUST 拒绝。

#### Scenario: 重复 start
- **WHEN** 宿主因回包丢失用同一 invocation_id 与相同参数重发 start
- **THEN** 执行端返回原 execution_id，不创建第二次执行

#### Scenario: 引用越权
- **WHEN** start 请求携带一个不属于当前 Company 的 context 引用
- **THEN** 宿主在发送前按授权解析失败并拒绝，不把任意 ref 解释为可读全公司

### Requirement: 接收状态与终态边界

执行状态 SHALL 包含 Accepted、Starting、Running、NeedsInput、NeedsAuth、Suspended、Cancelling、Completed、Failed、Cancelled 与 Unknown。Completed 只表示候选结果已提交，MUST NOT 表示工作包 Accepted；业务完成、传输退出与进程退出 MUST 分开观察，退出码 0 不得作为验收通过依据，终态后迟到的 detail 日志不得自动推翻权威终态。

#### Scenario: 进程退出但候选未验收
- **WHEN** 执行进程退出码为 0 且提交了一份候选成果，但必需检查尚未运行
- **THEN** 宿主把执行记为 Completed、把验收记为 Pending，不完成 Issue

### Requirement: 回包丢失先对账

开始请求回包丢失后 SHALL 先按 invocation_id 查询；只有声明持久去重、查询能力和保留期时宿主才可据此安全重发。无上述能力时 MUST 记录尝试并进入 Unknown，MUST NOT 新建 Attempt 规避未知状态。

#### Scenario: 黑盒服务回包丢失
- **WHEN** 一个不提供持久去重的外部服务未返回 start 结果
- **THEN** 宿主标记 Unknown 并保留占用与现场，等待查询、人工裁决或对账，不自动启动第二个执行

### Requirement: 观察信封与缺口

观察信封 SHALL 包含 invocation_id、source_epoch、stream_id、source_sequence、observed_at、type、payload 与来源。协商可靠观察时，ACK SHALL 只确认最高连续接收 sequence；收到后续序号但缺前序 MUST NOT 越过缺口确认。超过保留窗口返回 Gap 或需要快照，MUST NOT 静默跳到最新；ACK 不表示业务处理完成或成果 Accepted。

#### Scenario: 乱序观察
- **WHEN** 接收方收到 sequence 19 但缺少 18
- **THEN** ACK 最多到 17，缺口被记录并触发重放或对账，不把 19 当成连续已完成

### Requirement: 最小 Conformance 样本

宿主 SHALL 为协议提供离线 conformance 样本，至少覆盖重复 start、同 ID 参数变化、开始回执丢失、观察乱序与断档、进程退出、取消请求与确认分离、认证失效、超大日志与最终费用迟到。Fake Executor SHALL 只用于这些故障测试，不提供生产智能执行能力。

#### Scenario: 运行 conformance
- **WHEN** 在 P0 验证作业中执行最小 conformance 集合
- **THEN** 每个样本有可列出的命名用例与真实断言，Fake Executor 通过不产生任何支持矩阵中的生产能力声明
