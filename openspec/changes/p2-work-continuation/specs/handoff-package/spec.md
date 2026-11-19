# Spec Delta: handoff-package

## Purpose

定义跨执行者接续的能力等级、确定编码与事实边界，使部分现场也能被登记，但自动接续只承诺已验证的范围。

## ADDED Requirements

### Requirement: 接续能力等级

系统 SHALL 只按 L1 资料交接、L2 现场重建、L3 验证后接续三种等级声明 HandoffPackage；L3 MUST 限定于已测运行时、工具与任务组合。不承诺任意厂商无损切换、内部推理导出或自动迁移全部工具状态。

#### Scenario: 未测组合请求 L3
- **WHEN** 用户请求对一个未列入已测组合的运行时执行 L3 接续
- **THEN** 系统拒绝自动接续并说明缺少的组合证据，允许降级为 L1/L2 或人工处理

### Requirement: 规范编码与摘要

HandoffPackage SHALL 使用固定 UTF-8、键顺序、数字/时间格式、数组顺序与禁止重复键的规范编码；integrity.payload_digest MUST 对规范编码 payload 求值且不纳入自身递归计算。原始文件 SHALL 另按字节计算摘要，不能把文本规范化当原文件完整性证明。

#### Scenario: 跨语言校验
- **WHEN** 两个不同语言实现校验同一份交接包
- **THEN** 它们对规范编码得到相同 payload_digest，且原始字节摘要单独报告

### Requirement: 宿主事实与 Agent 叙述

交接骨架 SHALL 由版本化工作区、已登记成果、验证记录、Effect 与预算快照生成；Agent 可补充摘要、失败原因与建议，但 MUST NOT 覆盖宿主账本、确认外部终态或把无证据的 done 标为已接受。

#### Scenario: 自述完成但无证据
- **WHEN** 提交交接包的 Agent 声称工作已完成但缺少验证引用
- **THEN** 系统保留该叙述为 unverified，宿主事实仍显示未完成

### Requirement: 部分快照与版本绑定

崩溃、预算耗尽或强制停止时 SHALL 生成带 missing、来源与完整性状态的尽力快照；部分包 MUST 可持久登记供人工恢复。版本绑定 SHALL 使用实际输入/工作包/现场 revision 与 snapshot_seq，无关元数据变化 MUST NOT 造成工作失效。

#### Scenario: 崩溃后部分现场
- **WHEN** 执行端崩溃且无法提供完整内部状态
- **THEN** 系统保存带 missing 列表的快照并允许人工查看，自动接续按声明等级判定是否准入

### Requirement: 现场清单边界

清单 SHALL 说明 Git submodule/LFS、工具链与锁文件、必要服务、未解决冲突及未导出的外部会话状态；MUST NOT 导出明文密钥、凭据环境变量或未经允许的客户资料。大文件按授权引用，摘要存在不意味着内容仍可恢复。

#### Scenario: 清单含凭据环境变量
- **WHEN** 捕获的现场材料包含凭据环境变量值
- **THEN** 系统只保留 SecretRef 名称与摘要，不在交接包中写入值

### Requirement: 交接事件

系统 SHALL 记录 handoff.requested、captured、validated、accepted、blocked、resumed 事件及原因与来源；执行 Agent 只能提交当前工作交接候选，MUST NOT 获得全局 handoff 管理凭据。

#### Scenario: 重复 accept
- **WHEN** 同一个交接 accept 请求被重复提交
- **THEN** 系统返回同一结果且不创建第二份执行或第二份包
