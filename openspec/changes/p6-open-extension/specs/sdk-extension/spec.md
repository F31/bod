# Spec Delta: sdk-extension

## Purpose

定义第三方适配器与 SDK 的接入边界，使外部运行时在不修改宿主安装的前提下扩展能力，且支持范围由符合性证据决定。

## ADDED Requirements

### Requirement: 描述符与能力协商

适配器 SHALL 以版本化描述符声明身份、操作、配置 schema、平台、限制与能力分项；宿主 MUST 按能力握手协商协议版本，拒绝缺少必需能力的任务。旧适配器 SHALL 仍按原已测路径工作，不因新扩展版本失效。

#### Scenario: 缺少必需能力
- **WHEN** 一个旧适配器不支持新的委派扩展但具备原本地操作能力
- **THEN** 宿主拒绝新扩展任务并保留原本地任务路径，不要求升级才能继续旧工作

### Requirement: 安装边界

第三方扩展 SHALL 可安装而不修改宿主源码或安装内容；安装前 MUST 校验版本、来源、完整性与许可证，并记录适配器版本、配置版本与证据日期。未通过 conformance 的扩展 MUST NOT 标 Supported。

#### Scenario: 未通过 conformance 的扩展
- **WHEN** 一个新扩展只通过 schema 校验但缺少 conformance 报告
- **THEN** 系统允许以 Experimental 安装调试，但不进入调度候选且不标 Supported

### Requirement: 分项证据与 UI 一致

能力声明 SHALL 区分自报、探测、契约测试与真实任务证据；探测结果、调度准入和用户界面 MUST 一致显示同一限制。关键身份、授权或消息完整性失败时 MUST 禁用执行，不能用降级徽章继续派发。

#### Scenario: 界面与调度不一致
- **WHEN** 某能力在探测中标 unsupported 但在任务按钮上显示可用
- **THEN** 该状态视为缺陷，调度与界面按探测结果统一为不可用

### Requirement: 描述符 schema 与 fixtures

宿主 SHALL 为描述符与配置 schema 提供版本化 fixtures，并在安装、升级和运行前核对清单；可执行文件摘要或工具清单变化 MUST 触发重新评估，旧批准只对原清单有效。

#### Scenario: 工具清单变化
- **WHEN** 适配器的可执行摘要在升级后变化
- **THEN** 原批准失效并生成重新评估提案，不对新清单沿用旧授权

### Requirement: 出站材料范围

首次向外部服务或插件发送项目材料时 SHALL 展示项目、资料范围、接收服务与账号并保存决定摘要；接收方、范围或账号变化 MUST 重新判断。审计记录 SHALL 只含引用与摘要，不复制原文或凭据。

#### Scenario: 扩大发送范围
- **WHEN** 已批准发送摘要后请求改为发送完整源文件
- **THEN** 系统要求重新判断与授权，未获准时接收端零次收到材料
