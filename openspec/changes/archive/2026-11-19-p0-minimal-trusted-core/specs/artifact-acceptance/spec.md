# Spec Delta: artifact-acceptance

## Purpose

定义候选成果从提交到接受的证据边界，使接受决策绑定确定修订与验证规则，且实现者无法自行批准自己的工作。

## ADDED Requirements

### Requirement: 候选与验收分离

执行端 Completed SHALL 只产生候选成果；验收状态 SHALL 按 Pending、Verifying、Verified 推进，并可进入 Rejected、NeedsRevision 或 Waived。Waived MUST 记录权限主体、理由、适用版本与范围，且 MUST NOT 显示为测试已通过。

#### Scenario: 验收被豁免
- **WHEN** 有权限主体对某低风险成果执行 Waived
- **THEN** 系统记录主体、理由、版本与范围，界面显示「已豁免」而不是「验证通过」

### Requirement: 接受绑定确定修订

Acceptance SHALL 绑定工作包修订、输入基线、产物修订、验证规则以及目标环境或合并组合。任一条件发生影响性变化 MUST 产生新的验证需求；旧接受记录 SHALL 保留，但 MUST NOT 用来批准新版本。

#### Scenario: 旧接受不能批准新产物
- **WHEN** 一个工作包产生新的产物修订，但有人引用上一修订的 Acceptance 请求接受
- **THEN** 系统拒绝，要求针对新产物修订重新验证与接受

### Requirement: 不可隐式自批

接受主体 SHALL 按当前授权与 AcceptancePolicy 确定；实现 Agent MUST NOT 拥有隐式自批权。自动接受 MUST 有项目预先授权、必需检查通过、证据与实际版本匹配且无阻塞风险；生产写入、不可逆操作与高风险项 SHALL 要求独立人工批准。

#### Scenario: 实现者自报通过
- **WHEN** 实现 Agent 提交候选成果并声称所有检查通过
- **THEN** 系统只登记候选与自报，按预先授权的检查结果决定是否接受，不因自报改变接受主体

### Requirement: 不可变成果修订

Artifact 与 Revision SHALL 使用稳定身份与不可变修订；CAS 写入 MUST 先写临时文件、计算 digest、原子转正，再在事务中登记引用。崩溃孤儿 SHALL 由可审计 GC 在宽限期内分批清理，外部仓库对象 MUST NOT 被本地 GC 自动删除。

#### Scenario: 崩溃后孤儿文件
- **WHEN** 宿主在 CAS 文件落盘后、引用登记前崩溃
- **THEN** 该对象不被当作有效成果，GC dry-run 报告为孤儿，外部仓库对象不受影响

### Requirement: 多工作包完成条件

Issue SHALL 只在必需工作包被接受且终结门禁通过后进入完成；单个 Agent 的完成报告 MUST NOT 完成整个 Issue。新输入、新规格或新验收条件 MUST 形成工作包修订与新 Attempt，而不是覆盖历史约定。

#### Scenario: 部分工作包已接受
- **WHEN** 一个 Issue 含两个必需工作包，其中一个已接受而另一个仍 Running
- **THEN** Issue 保持进行中，系统列出未完成工作包与阻塞原因

### Requirement: 决策记录

Decision SHALL 记录实际 actor、scope、决定、原因、关联证据与修订引用。问题回答、规格提案与反馈 MUST NOT 静默改变需求、权限或验收；影响这些条件的答案 MUST 走各自修订或授权流程。

#### Scenario: 答疑改接口
- **WHEN** 一次普通答疑给出新的接口行为并被执行端当作事实
- **THEN** 宿主不把它当作已批准规格，要求提交规格修订并经负责人接受后才影响验收
