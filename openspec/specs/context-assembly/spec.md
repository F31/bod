# context-assembly Specification

## Purpose
定义单次 Attempt 的确定上下文装配与指令边界，使同一规格在真实工作区可复现，且角色、缓存或外部材料不能扩大权限。

## Requirements

### Requirement: ContextBundle 清单

每次 Attempt SHALL 固定一份 ContextBundle，至少记录 context_bundle_id、attempt_id、spec_revision、profile_revision、skill_refs、preference_refs、input_refs、adapter_version、runtime_config_hash、assembly_version 与 omitted_refs 及省略原因。所有引用 MUST 指向确定修订或摘要。

#### Scenario: 复现一次执行
- **WHEN** 用户用相同修订与运行时配置重放一次 Attempt 的上下文装配
- **THEN** 系统生成相同清单与摘要，可解释每个 omitted_ref 的原因

### Requirement: 不可变修订复用

Profile、Skill、规格解析与稳定上下文片段 SHALL 按不可变 revision/digest 复用；命中缓存时 MUST 重新检查当前 Company、Project 与主体权限。内容变化 MUST 产生新键，不覆盖旧内容。

#### Scenario: 撤权后命中旧缓存
- **WHEN** 某成员被撤权后请求复用其上次解析的上下文片段
- **THEN** 缓存命中被拒绝并重新按当前权限装配，不返回旧片段

### Requirement: 指令优先级与冲突

装配 SHALL 按用户目标与验收、项目授权规则、Profile、Preference、外部材料的顺序应用指令；外部网页、工具输出、Issue 正文与反馈 MUST 标来源与不可信属性。冲突 MUST 形成 Question 或待决定，不得静默覆盖高优先级约束。

#### Scenario: 工具输出要求改验收
- **WHEN** 工具输出声称验收标准应被放宽
- **THEN** 系统保留其不可信标记并形成 Question 或待决定，原验收标准不变

### Requirement: 会话复用与角色变更

会话复用 SHALL 要求相同 Company、Project、输入修订与有效权限；角色、Profile 或授权变化 MUST 新建会话或执行显式可审计重置。共享 Agent 在不同项目的会话、工作区与密钥 MUST 分离。

#### Scenario: 同一 Agent 跨项目
- **WHEN** 一个 Agent 同时被指派到两个项目
- **THEN** 两个 Attempt 使用不同会话、工作区与 ContextBundle，不复用敏感历史

### Requirement: 审计与隐私

装配记录 SHALL 保存引用摘要、来源、时间与省略原因；MUST NOT 把密钥、凭据环境变量或不必要的个人信息写入 ContextBundle 或审计正文。

#### Scenario: 审计不含密钥
- **WHEN** 用户导出一次 Attempt 的上下文装配记录
- **THEN** 记录引用 SecretRef 名称与摘要，不包含密钥值或个人敏感原文
