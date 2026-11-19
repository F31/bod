# Spec Delta: workspace-execution

## Purpose

定义一次 Attempt 的隔离工作区与修改范围，使真实仓库中的候选成果可追溯，且不通过共享可写目录污染其他执行或证据。

## ADDED Requirements

### Requirement: 每 Attempt 独立工作区

系统 SHALL 为每个 Attempt 绑定唯一工作区与分支，独立暂存区；不同 Agent 或 Attempt MUST NOT 共用可写目录、暂存区或把彼此未提交内容当正式输入。共享对象缓存 SHALL 只读且受凭据隔离。

#### Scenario: 并行 Attempt
- **WHEN** 两个 Attempt 同时在同一逻辑仓库执行不同工作包
- **THEN** 它们使用不同 worktree 与分支，互不读取未提交内容，共享缓存只读

### Requirement: 写范围与路径门禁

派发前 SHALL 冻结 write_scope 与 coordinated_scope；提交候选时 MUST 用 diff 校验写范围，检查软链接穿越、Git 管理目录访问与共享配置修改。超出范围的改动 MUST 被拒绝并进入 NeedsRevision。

#### Scenario: 越界修改共享配置
- **WHEN** 执行端修改了未授权的根配置文件
- **THEN** 门禁拒绝该候选并列出越界路径，不把它作为可接受的交付修订

### Requirement: 共享资源协调

API、公共类型、事件 schema、migrations、根依赖清单与 OpenSpec 文件 SHALL 由明确协调者处理，变更需影响分析；同一迁移 ID MUST 统一分配并检查顺序与并发兼容。模块内代码和测试可由工作包独立修改。

#### Scenario: 两个工作包改同一迁移
- **WHEN** 两个 Attempt 各自生成相同编号的 migration
- **THEN** 合并门禁阻止冲突，要求统一分配 ID 并重新检查顺序与回退兼容

### Requirement: 工作区生命周期

工作区 SHALL 绑定 attempt_id、租约与保留策略；进入终态后按策略回收，但脏文件、未确认外部作用与有保留价值的候选 MUST 先登记引用或得到明确处置。用户仓库、生产环境 MUST NOT 因工作区清理被删除。

#### Scenario: 取消后仍有脏文件
- **WHEN** 用户取消一个 Attempt 且工作区存在未提交改动
- **THEN** 系统保留并登记该现场，显示待核实项，不自动清空也未删除外部仓库

### Requirement: 环境隔离

测试端口、临时目录、数据库与可变构建输出 SHALL 按工作区隔离；共享缓存 MUST 支持并发且不产生跨 Company 数据泄露。

#### Scenario: 并发测试端口冲突
- **WHEN** 两个工作区同时启动本地测试服务
- **THEN** 各自使用隔离端口与临时目录，不互相占用或读取对方数据
