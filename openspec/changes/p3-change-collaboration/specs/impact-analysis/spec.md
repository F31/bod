# Spec Delta: impact-analysis

## Purpose

定义规格、接口、源码、测试与环境之间的显式影响关系，使变化后能找出受影响成果并生成有限、有预算的重验提案，而不是全量重跑或假装无影响。

## ADDED Requirements

### Requirement: 依赖边分类

Revision.depends_on SHALL 指向上游确定修订，并使用 requires、implements、verifies、built_from、deployed_to 等关系类型；每条边 MUST 记录来源、revision、适用范围。显式边与 Agent 推测边 MUST 分开，推测边带依据与置信，不能直接变成确定事实。

#### Scenario: 推测边不升级
- **WHEN** Agent 根据命名相似度推测两个组件存在依赖
- **THEN** 系统记录为推测边并附依据与置信，不据此自动标记下游失效

### Requirement: 影响评估内容

上游变化后 SHALL 生成 ImpactAssessment，列出确定受影响、可能受影响、不在已知影响范围与缺少关系的区域；「未找到依赖」MUST NOT 表示已证明不受影响。评估 MUST 固定旧/新基线、关系快照、规则版本、触发事件与分析时间。

#### Scenario: 部分区域未知
- **WHEN** 一个接口 schema 变化且某组件没有可解析的依赖边
- **THEN** 评估把该组件列为缺少关系/影响未知，而不是不受影响

### Requirement: 选择性重验

系统 SHALL 先按确定性关系计算候选范围，再按需要交给已有 Agent 分析语义影响；受影响成果产生重验建议和待办草案，经授权策略或人工接受后才创建新工作或验证计划。自动删除成果、修改旧接受或无限触发新任务 MUST 被禁止。

#### Scenario: 接口变化跨组件
- **WHEN** 一个配额接口变更影响 API、前端表单、SDK 与权限场景
- **THEN** 系统只生成这四个区域的重验提案，无关监控报告不进入候选

### Requirement: 重验有界与去重

重验与维护工作 SHALL 有幂等键、预算、范围与停止条件；去重键 MUST 关联变更与评估修订，并发变化合并或明确排队。安全、公共迁移、工具链、环境或跨组件契约变化 SHALL 触发更广验证或人工判断，不能因成本理由忽略明确门禁。

#### Scenario: 重复变化事件
- **WHEN** 短时间内收到多个指向同一接口修订的变化事件
- **THEN** 系统合并生成一个重验提案，不重复派发或无限循环

### Requirement: 维护提案

条件变化 SHALL 生成有限维护工作提案，默认只读检查；断网或缺失观测显示 unknown/stale，不把无告警当健康。旧 Agent 停用时由项目责任人处理提案，不依赖永久在线经理。

#### Scenario: 环境变化触发检查
- **WHEN** 部署环境依赖版本变化但观测不可达
- **THEN** 系统生成待核实维护提案并标 unknown，不自动升级或发布
