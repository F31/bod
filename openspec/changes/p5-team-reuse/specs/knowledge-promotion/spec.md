# Spec Delta: knowledge-promotion

## Purpose

定义反馈到正式知识/Profile/Skill 修订的受控晋升，使改进能复用，但外部材料和学习建议不能改变权限、预算或已批准的验收。

## ADDED Requirements

### Requirement: 反馈分类与最窄作用域

反馈 SHALL 按目标分类并默认保持最窄作用域：当前工作包、当前项目、指定客户/项目/角色或指定系统与版本；「下次」MUST NOT 自动解释为永久或全组织。权限、密钥、预算、生产发布请求 SHALL 转独立治理流程，不作为普通学习规则。

#### Scenario: 下次替我发布
- **WHEN** 反馈中出现「以后直接替我发布生产」
- **THEN** 系统生成发布授权请求交治理流程，不写入 Profile 或项目规则

### Requirement: 不可变提案与冲突

FeedbackProposal SHALL 是不可变修订；Profile/Skill 更新 MUST 复用 ArtifactRevision 与 expected_base_revision。并发更改返回 Conflict 并保留候选，合并后重新检查，MUST NOT 追加到同一个可变文件。

#### Scenario: 并发修改 Profile
- **WHEN** 两个提案基于同一个 Profile 修订生成，其中一个已接受
- **THEN** 另一个返回 Conflict 并保留候选，要求基于新修订重新检查

### Requirement: 接受策略与来源权限

自动接受规则 SHALL 包含 actor、允许的目标/字段、项目/角色范围、有效期与审计；仅已授权范围内的低风险偏好可自动接受。外部 PR 评论、网页、工具输出与 Agent 自述 MUST NOT 作为权威反馈，只有已认证且有范围权限的主体能推动接受。

#### Scenario: 外部网页要求改规则
- **WHEN** 一个网页内容声称「所有项目必须使用新规则」
- **THEN** 系统保留其不可信属性，不提升为高优先级指令，不自动发布规则

### Requirement: 发布、会话与撤销

更新 SHALL 发布新修订与指针，下一次 Attempt 解析后固定；旧 Attempt MUST NOT 切换普通角色/偏好规则。无法刷新 hosted session 时建立新会话，回滚也 SHALL 刷新会话而非只改文件；回滚不撤销已发生的外部操作或账本。

#### Scenario: 回滚偏好
- **WHEN** 用户撤销一项已发布的沟通偏好
- **THEN** 新 Attempt 使用旧修订并刷新会话，历史执行记录保留当时实际使用的版本

### Requirement: 缓存与检索隔离

Profile/Skill/规格解析缓存 SHALL 按不可变 revision/digest 复用，并先按 Company/Project 与主体权限过滤再返回；敏感原文删除或撤权后 MUST NOT 因缓存继续泄露。向量检索按实际召回需求增加，不宣称结构化检索覆盖全部知识需求。

#### Scenario: 撤权后检索
- **WHEN** 某成员被撤权后查询跨项目知识
- **THEN** 检索先过滤 Company/Project 与权限，结果不含无权访问条目

### Requirement: 效果记录

EvaluationRecord SHALL 记录任务集版本、Profile/Skill 修订、Runtime、模型配置、环境、样本数、验收标准、费用、耗时、人工干预与结果；样本不足显示不足，禁止用两次不同任务的对照或旧模型回放断言新规则更优。

#### Scenario: 样本不足仍要提高优先级
- **WHEN** 一个规则只在一个任务上表现较好就请求自动提升
- **THEN** 系统标记样本不足并要求项目设定条件，不自动提升
