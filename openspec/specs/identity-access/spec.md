# identity-access Specification

## Purpose
定义本地单宿主中 Company、Project、Agent 与 Issue 的身份和授权边界，使每次业务写入由当前有效权限决定，执行端反馈不能自报越权。

## Requirements

### Requirement: 默认 Company 与归属

系统 SHALL 在首次启动提供一个默认 Company；Issue 属于一个 Company 并可关联 Project，Coding Issue MUST 具备项目与仓库上下文。跨 Company 查询 MUST 携带 company_id，不得把不同库的事件序号当统一因果关系。

#### Scenario: 首次启动与建单
- **WHEN** 用户在空安装中启动宿主并创建一个 Project，随后提交一个关联该仓库的 Issue
- **THEN** 系统创建默认 Company，Issue 记录 company_id 与 project_id，且跨 Company 查询必须显式给出 company_id

### Requirement: 直接指派

系统 SHALL 允许用户把 Issue 直接指派给已接入的 Agent 或人，且不要求 Team、TeamMembership、ProjectTeamBinding 或组织层级存在。任务级只有一个 accountable owner，贡献者可以多个。

#### Scenario: 无 Team 交办
- **WHEN** 用户在一个只有 Company、Project 和一个已接入 Agent 的实例上直接指派 Issue
- **THEN** 指派成功并生成唯一责任人与 Assignment，系统不要求任何 Team 配置

### Requirement: 权限交集与当前复核

受管动作 SHALL 在每次执行前取角色授权、ProjectAccessGrant、Issue Assignment、工作包范围、运行授权与即时撤权的交集；低层配置、资源归属或角色称号 MUST NOT 扩大权限。权限被撤销后 MUST 阻断后续受管动作。

#### Scenario: 撤权后立即拒绝
- **WHEN** 某 Agent 的项目授权被撤销后，其沿用旧 Binding 尝试认领新的工作包执行
- **THEN** 宿主以 InsufficientPermission 拒绝，记录实际 actor、scope 与授权版本，且不创建新的执行租约

#### Scenario: 角色称号不授权
- **WHEN** 执行端在反馈中自称管理员，或某资源 ownerReference 指向该 Agent
- **THEN** 宿主不据此扩大其工具、预算或发布权限，并按当前 Grant 判定

### Requirement: 执行反馈与主体身份分离

系统 SHALL 只接受来自已认证 invocation 身份的执行反馈；反馈 payload 中自报的 Company、Project 或 Manager 身份 MUST NOT 作为授权依据。管理命令与执行反馈 MUST 走不同的受权入口。

#### Scenario: 伪造项目归属的反馈
- **WHEN** 一个绑定项目 A 的 invocation 提交带有项目 B 标识的成果或管理命令
- **THEN** 系统按 invocation 实际绑定范围处理并拒绝越权部分，记录拒绝原因，不把 payload 自报当作授权

### Requirement: 拒绝可解释与审计

权限拒绝 SHALL 返回稳定原因码与重新激活条件，并记录实际主体、权限依据和对应修订；一人兼任多个职责时 MUST 逐项记录。审计与账本不得因界面简化而丢失。

#### Scenario: 审计兼任职责
- **WHEN** 同一用户在个人模式下同时批准范围与接受成果
- **THEN** 两条决定分别记录同一真实身份、各自 scope 与修订，界面可合并展示但不覆盖审计事实

### Requirement: 本地部署边界

默认部署 SHALL 只通过回环地址或 UDS 暴露服务；Web 默认只监听本地，启用远程访问 MUST 显式开启认证与 TLS。不可信多租户不在本能力支持范围。

#### Scenario: 默认监听范围
- **WHEN** 用户以默认配置启动宿主
- **THEN** 控制面只绑定本地地址，未显式配置认证与 TLS 前不接受远程访问
