# Spec Delta: runtime-verification

## Purpose

定义验证计划、构建记录与实际运行验收的证据绑定，使质量门禁结果只在其被测制品、环境和规则版本上有效。

## ADDED Requirements

### Requirement: VerificationPlan

系统 SHALL 把 Scenario/Criterion 映射到可执行检查、工具、数据、环境与预期结果；支持现有脚本/CI、API 检查、浏览器流程、测试 Agent 与人工检查，MUST NOT 自建浏览器引擎、测试语言或模型循环。

#### Scenario: 计划覆盖核心场景
- **WHEN** 为一个候选交付建立验证计划
- **THEN** 计划至少覆盖应用启动/健康、核心 API、权限与数据读写、一条关键 UI 流程与失败路径，并标明每项的检查方式

### Requirement: 证据绑定

每个验证报告 SHALL 记录被测 commit/digest、spec/criterion 修订、环境、工具/规则版本、执行者、时间与原始报告引用；只有与候选交付匹配的证据可用于接受。缺失或不匹配 MUST 标 Unknown/NeedsRevalidation。

#### Scenario: 证据与目标不匹配
- **WHEN** 一份测试报告来自候选分支的旧提交，而最终制品由合并后提交构建
- **THEN** 该报告不用于接受，系统要求重新验证或记录等价关系，不能只凭分支绿灯接受

### Requirement: 构建记录与晋升

BuildRecord SHALL 记录源码 commit、构建定义版本、工具链、依赖锁、平台、参数摘要、执行环境与输出 digest，并声明是否验证可复现。同一候选 SHALL 尽量一次构建、按 digest 在环境间晋升；平台特定重建 MUST 生成新 BuildRecord 与验证。

#### Scenario: 按 digest 晋升
- **WHEN** 一个已验证制品从测试环境晋升到预发布环境
- **THEN** 系统复用同一 digest 与验证证据，不重新构建不同字节的包

### Requirement: 门禁结果语义

检查分为 Required、Advisory、Manual，结果分为 Pass、Fail、Unknown、NotApplicable；未运行 MUST NOT 默认 Pass。必需项 Unknown 或 Fail 时 MUST NOT 自动 Accepted，豁免 SHALL 单独记录。

#### Scenario: 必需项未运行
- **WHEN** 一个必需权限检查没有执行记录但其他检查通过
- **THEN** 结果为 Unknown，自动接受被阻止，界面列出未运行项与原因

### Requirement: 独立性与有效期

独立验证 SHALL 体现证据来源、权限或方法的独立性，不能只更换角色名；自生成实现与自生成测试可能共享错误假设时必须引入基准、契约用例或人工抽样。验证结论 SHALL 只在固定输入、环境与规则条件下有效，环境或规则变化触发重验判断。

#### Scenario: 规则版本变化
- **WHEN** 验证规则升级而制品与环境不变
- **THEN** 旧结论标为需重验，不宣称「测试永远有效」

### Requirement: 失败分类

检查结果 SHALL 区分前置失败、环境失败、应用失败与 unknown；截图和日志是辅助观察，MUST NOT 仅凭界面出现判断数据正确或权限安全。测试不得使用未经授权的生产数据或自动写生产系统。

#### Scenario: 测试基础设施故障
- **WHEN** 运行环境不可用导致检查失败
- **THEN** 系统记为环境失败而非应用缺陷，不把该结果当代码质量结论
