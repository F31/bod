# Spec Delta: release-governance

## Purpose

定义环境引用、发布计划、审批与回滚边界，使发布动作绑定实际制品和授权，未知环境状态与不可回退资源被如实表达。

## ADDED Requirements

### Requirement: 环境引用与状态投影

EnvironmentRef SHALL 包含 provider、external_id、用途、访问范围、生命周期、desired_revision、observed_revision、observed_at 与 drift；密钥 MUST NOT 进入 Manifest 明文。环境的期望与实际 SHALL 分开，平台管资源，bod 管需要哪个版本、允许谁操作及结果是否满足验收。

#### Scenario: 观测漂移
- **WHEN** 环境实际版本与期望修订不一致
- **THEN** 系统记录 drift 与观测时间，不把期望状态显示为已部署

### Requirement: ReleasePlan 审批

ReleasePlan SHALL 固定 Manifest、目标环境、部署方式、迁移、验证、停止条件与回滚依据；审批 MUST 绑定实际制品 digest、环境、参数与有效期，目标变化后原审批失效。

#### Scenario: 审批后改目标
- **WHEN** 审批通过后用户把测试目标改为生产
- **THEN** 原审批失效，宿主拒绝待发部署并要求重新批准

### Requirement: 部署状态与发出请求分开

部署状态 SHALL 使用 Pending、Running、Verifying、Succeeded、Failed、Unknown；发出部署请求 MUST NOT 等于已运行成功。外部动作在提交后由 Effect Gate 派发，回包丢失先按 invocation 或业务键查询。

#### Scenario: 部署回包丢失
- **WHEN** 连接器已收到部署请求但未返回结果
- **THEN** 宿主显示 Unknown 并保留风险占用，不自动重发部署

### Requirement: 回滚边界

回滚 SHALL 只恢复已声明可回退资源；数据库不可逆迁移、外部消息与数据写入 MUST 标明需前向修复。自动回滚 SHALL 仅按经过验证和授权的策略执行。

#### Scenario: 不可逆迁移
- **WHEN** 发布包含一个不可逆数据库迁移并要求回滚
- **THEN** 系统声明该资源不可回退并给出前向修复方案，不声称已恢复原状态

### Requirement: 预算解释与发布门禁

发布前 SHALL 展示已结算、有效预留、未知风险占用、可用额度与计量来源，并核对成本是否按实际 invocation 记账。预算或审批未满足时 MUST 阻止新发布，已接受成果与历史费用保留。

#### Scenario: 预算不足阻止发布
- **WHEN** 一个已验收 Manifest 因预算耗尽无法发布
- **THEN** 系统阻止新发布、显示拒绝原因与批准入口，不修改验收或历史账本

### Requirement: 维护触发

MaintenancePlan SHALL 关联正式 Manifest，记录维护主体、前提、最后验证、触发条件、允许检查、预算、停止条件与人工接管。变化后 SHALL 生成有限工作提案，默认只读检查，不自动开启无限开发、升级或生产发布。

#### Scenario: 依赖到期
- **WHEN** 一个依赖版本到期触发维护检查
- **THEN** 系统生成待审阅提案并限制预算与范围，不自动升级生产
