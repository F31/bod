# Spec Delta: multi-agent-workspace

## Purpose

定义单仓库多智能体的工作区、共享资源协调与受控合并，使各分支候选能在最新主干组合上验证后再被接受。

## ADDED Requirements

### Requirement: 独立工作区与分支

一个逻辑仓库 SHALL 支持多个独立 worktree，每个 Attempt 使用独立分支与暂存区；主干只能通过受控合并入口更新。多个 Agent MUST NOT 操作彼此暂存区或读取彼此未提交内容作为正式输入。

#### Scenario: 分支命名与隔离
- **WHEN** 两个工作包分别生成候选
- **THEN** 它们使用各自分支与 worktree，主干 ref 只在受控合并入口更新

### Requirement: 共享资源协调

模块内代码与测试可由工作包独立修改；API、公共类型、事件 schema、migrations、根依赖清单与 OpenSpec/任务映射 SHALL 由明确责任者协调，变更需影响分析。生成代码 MUST 由确定输入与生成器版本生成，避免手工多处修改。

#### Scenario: 共享 schema 被两包修改
- **WHEN** 两个工作包各自修改同一公共 schema
- **THEN** 契约冲突门阻止合并，要求责任者协调并重新验证

### Requirement: 合并队列与最新基线

候选合并 SHALL 串行化共享生效点：在最新目标上构造候选合并结果，再运行编译、测试与评审，最后带期望目标 commit 更新 ref。主干在候选验证期间被更新时 MUST 重建候选验证，不得沿用旧组合结果。

#### Scenario: 主干在验证期间更新
- **WHEN** 候选分支测试通过后主干获得新提交
- **THEN** 系统重建合并候选并重新验证，旧通过结果不能用于最终接受

### Requirement: 合并证据

每个合并 SHALL 记录实际目标 commit、候选组合、验证命令与结果、冲突处理、期望与实际更新 ref。合并证据 MUST 对应真实组合，评审自述或单分支绿灯 MUST NOT 作为证据。

#### Scenario: 证据与目标不匹配
- **WHEN** 合并证据记录的 target commit 与实际更新 ref 不同
- **THEN** 系统拒绝接受该合并并要求重新验证

### Requirement: 写入边界与隔离声明

路径限制 SHALL 通过 diff 校验；要形成安全边界，runner MUST 限制写路径、软链接穿越与 Git 管理目录访问。worktree 只能标为协作隔离，不能标为安全沙箱；需要强隔离时使用独立 clone、容器或远程工作区及受限凭据。

#### Scenario: 声明强沙箱
- **WHEN** 某适配器只使用 worktree 隔离却声明 sandboxed
- **THEN** 系统拒绝该声明并降级为协作隔离，限制其承接要求强隔离的任务
