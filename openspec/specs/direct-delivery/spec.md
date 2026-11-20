# direct-delivery Specification

## Purpose
定义无需 Team、WorkPackage 或 Run 配置的真实仓库直派闭环，使候选成果从规格到人工接受可追踪，且取消与 Unknown 状态如实呈现。

## Requirements

### Requirement: 首次使用路径

系统 SHALL 允许用户按「选项目或仓库 → 说清目标 → 查看范围与验收 → 交办」完成首次派发，自动补齐最小工作包与运行实例；MUST NOT 要求 Team、组织图、WorkPackage 或 Run 配置。

#### Scenario: 新项目首次交办
- **WHEN** 用户在一个只含项目和已接入 Agent 的实例上提交目标与验收条件
- **THEN** 系统生成默认工作约定并完成指派，不提示配置 Team 或执行图

### Requirement: 真实仓库交付闭环

系统 SHALL 支持在同一规格修订下完成真实仓库的实现与本地验证，提交候选成果，按人工或预授权策略接受；工作包修订、输入基线、产物摘要与验证规则 MUST 绑定。关闭桌面 SHALL NOT 丢失工作或终止已有宿主。

#### Scenario: 一次完整交付
- **WHEN** 用户对真实仓库指派一个修复任务，执行端提交候选且本地验证通过，验收人接受
- **THEN** 系统记录绑定修订的接受决定与交付证据，重启桌面后仍可查询该交付

### Requirement: 取消与 Unknown 如实显示

请求取消 SHALL 与确认停止分开；未确认停止 MUST 保留风险占用、工作区与待核实项。回包丢失或外部结果不明时 MUST 显示 Unknown/待核实与处理人，不得生成新 Attempt 规避未知状态。

#### Scenario: 取消回包丢失
- **WHEN** 执行端只返回 cancel_ack 而未确认停止
- **THEN** 宿主显示停止未确认，保留占用与现场，并列出对账步骤

### Requirement: 本地验证与备份

接受前 SHALL 运行与候选交付匹配的本地验证，报告记录被测 commit/digest、规则版本、环境、执行者与时间；备份 SHALL 可核验并用于恢复演练。退出码 0 或 Agent 自述通过 MUST NOT 作为接受证据。

#### Scenario: 自报通过但无报告
- **WHEN** 执行端声称测试通过但没有绑定实际提交的报告
- **THEN** 验收保持 Pending/NeedsRevalidation，不产生接受决定

### Requirement: 工作台投影

工作台首页 SHALL 呈现本期目标、下一项可交付成果、阻塞点与必须由用户决定的事项；任务页 MUST 显示当前状态、负责人、证据与拒绝原因，且只从宿主事实计算，不建立第二个业务写入者。

#### Scenario: 待决定清单
- **WHEN** 一个工作包因预算或澄清被阻塞
- **THEN** 首页待决定列出该事项、阻塞主体、所需决定与可采取动作，任务页显示对应证据

### Requirement: 预算阻止新派发

当预算约束触发时 SHALL 阻止新派发并给出稳定原因与解除条件；已接受成果与已结算费用 MUST 保留，不因停止新工作而被改写。

#### Scenario: 额度耗尽
- **WHEN** 一个项目的可用额度为零且仍有就绪 Issue
- **THEN** 系统显示 InsufficientBudget 与批准入口，不创建新 Binding，也不清理已接受交付
