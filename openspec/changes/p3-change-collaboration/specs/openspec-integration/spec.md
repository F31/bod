# Spec Delta: openspec-integration

## Purpose

定义 OpenSpec 规格、任务与 bod 工作包之间的确定映射，使多个 Agent 共享一份规格基线，且进度勾选不被误当成验收证据。

## ADDED Requirements

### Requirement: 变更与规格修订映射

系统 SHALL 把 Change 目录映射到带 repository 与路径的 Change 对象，并把系统规格与需求场景映射到 SpecRevision、RequirementRef 与 ScenarioRef；工作包 MUST 引用确定修订，而不是浮动最新文件。

#### Scenario: 工作包派生
- **WHEN** 一个已批准 Change 产生工作包
- **THEN** 工作包记录 spec_commit、需求/场景引用与输入修订，后续文件改动不静默改变其输入

### Requirement: 稳定任务身份

tasks.md 条目 SHALL 映射到稳定 TaskDefinitionRef，映射记录来源路径、锚点、指纹与匹配状态。文本重排或语义变化无法唯一匹配时 MUST 进入 Unresolved 并人工确认，MUST NOT 按行号或显示编号自动绑定。

#### Scenario: 任务重排
- **WHEN** tasks.md 条目顺序变化但内容相同
- **THEN** 稳定 ID 保持不变；内容语义变化导致无法匹配时标 Unresolved 并要求人工确认

### Requirement: 工作包拆分与覆盖检查

拆分 SHALL 先按业务行为、可独立验收与交付边界决定，再按契约、模块和验证边界划工作包；必须检查需求覆盖、循环依赖、共享资源冲突与工作包大小。不得按 Agent 数量或行范围机械均分。

#### Scenario: 机械均分被拒绝
- **WHEN** 计划器试图仅按 Agent 数量平均分配任务行
- **THEN** 宿主拒绝该计划并要求按可验证输出重新划分

### Requirement: 进度投影与验收分离

规格和计划任务定义 SHALL 以 Git 文件为准；工作包依赖、运行、授权与验收以 bod 为准。tasks.md 勾选只是进度投影，MUST NOT 作为单独验收证据；外部手工勾选导入为观察，不自动把工作包标为 Accepted。

#### Scenario: 手工勾选后被导入
- **WHEN** 某 Agent 在工作树中勾选了 tasks.md 条目并提交
- **THEN** 系统只登记为观察，工作包仍按宿主证据与接受决定判定

### Requirement: 单一归档入口

工作包全部接受 SHALL NOT 直接完成 Change；必须经过集成验收与规格/实现一致性检查，再由单一归档协调入口同步规格与归档。多个 Change 修改同一 requirement 时 MUST 串行协调，不仅按 Git 文本无冲突判断。

#### Scenario: 多 Change 改同一需求
- **WHEN** 两个 Change 都修改了同一个 requirement 且文本无冲突
- **THEN** 归档入口串行协调两者的规格增量，不允许并行写入现行规格
