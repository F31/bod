# coordination-loop Specification

## Purpose
定义持久化的唤醒去重与资源队列，使通知丢失、重复事件和宿主重启时每个资源按代数推进，且运行中出现的新工作不会被合并丢失。

## Requirements

### Requirement: 唤醒回执去重

系统 SHALL 以 (company_id, producer_id, idempotency_key) 作为 wake_receipts 逻辑唯一键。同键同参数 MUST 返回原回执；同键不同参数 MUST 拒绝；在声明保留窗口内，即使对应工作已终态也必须继续去重。定时触发键 MUST 绑定持久 schedule_id 与 occurrence_id，重启不得生成伪装重试的新请求。

#### Scenario: 重复命令返回原回执
- **WHEN** 客户端因超时用同一逻辑键与相同参数重复提交唤醒请求
- **THEN** 系统返回原回执与结果引用，不创建第二个 Attempt 或第二条派发意图

#### Scenario: 同键改参数
- **WHEN** 客户端复用逻辑键但改变输入摘要
- **THEN** 系统拒绝并说明键冲突，要求使用新的逻辑动作键

### Requirement: 运行中新工作不丢失

接受新逻辑唤醒时 SHALL 在同一事务创建回执并将对应资源的 wanted_generation 加一；重复回执 MUST NOT 再次递增。输入事件、问题与任务事实 MUST 先持久保存，新唤醒只引用这些事实，不得只保留最后一个 payload。

#### Scenario: 执行中收到新事件
- **WHEN** 某资源正在被消费时收到一个新的不同逻辑唤醒
- **THEN** wanted_generation 递增且状态保持 running，新事件事实被持久记录，完成本轮后继续处理新增代数

### Requirement: 原子认领与租约

队列消费 SHALL 用条件更新验证 pending、到期时间与租约 token，成功后保存 claimed_generation = wanted_generation 并递增 lease_token。同一资源同时只有一个有效消费者；完成确认 MUST 匹配 lease_owner 与 lease_token，若 wanted_generation 大于 claimed_generation 则转 pending，否则转 idle。

#### Scenario: 并发认领
- **WHEN** 两个消费者同时尝试认领同一个就绪资源
- **THEN** 只有一个条件更新成功并持有租约，另一个读取当前状态后退出或等待，不产生并行执行

#### Scenario: 完成后发现新代数
- **WHEN** 租约持有者确认本轮完成时 wanted_generation 已大于 claimed_generation
- **THEN** 资源转为 pending 并安排下一轮处理，不被标记为 idle

### Requirement: 租约过期与已派发动作

租约过期 SHALL 先恢复处理权；若前轮已发起 Agent 调用或部署，MUST 先检查已有 invocation 或 effect，不得因队列租约过期直接再执行。租约 token MUST 持久化且单调递增，旧 token 不得更新宿主受管状态。

#### Scenario: 租约过期但外部动作已发出
- **WHEN** 消费者租约过期，而对应 start 请求已发出但回包未收到
- **THEN** 系统按 invocation 查询外部状态，无法确认则进入 Unknown 与对账，不生成新 invocation

### Requirement: 稳定等待与有界退避

控制器 SHALL 以已提交状态和外部观测计算下一步；瞬时错误使用有界退避与抖动，权限、输入或预算阻塞进入稳定 Blocked，超过重试预算进入 dead 并保留原请求与原因。MUST NOT 在策略不允许时形成热重试，也不得在未对账前重复启动。

#### Scenario: 预算阻塞
- **WHEN** 一个就绪工作包因预算不足被拒绝
- **THEN** 控制器设置 InsufficientBudget 稳定状态、阻塞原因与重新唤醒条件，不周期性重启该工作包

### Requirement: 重启恢复

事件回放 SHALL 只用于恢复事实投影，MUST NOT 在回放中调用外部系统。重启后的协调 MUST 扫描未终结资源与 outbox，再查询执行端或第三方状态，然后决定推进、等待或对账。

#### Scenario: 提交后崩溃
- **WHEN** 宿主在 outbox 意图提交后、外部调用发出前崩溃并重启
- **THEN** 系统从持久记录恢复意图，确认外部状态后再派发或释放未发部分，不重复扣减预算

### Requirement: 队列可观测

系统 SHALL 提供队列深度、最老到期/未处理时间、代数滞后、租约过期数、dead 数、重试次数与投递延迟的可查询结果，供 doctor 与人工接管使用。

#### Scenario: 诊断积压
- **WHEN** 运维人员运行 doctor 查看队列健康
- **THEN** 输出上述指标与最老未处理资源标识，并把 Unknown 与 dead 项单列
