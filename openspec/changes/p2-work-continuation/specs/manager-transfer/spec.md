# Spec Delta: manager-transfer

## Purpose

定义管理负责人切换时的授权纪元、旧命令拒绝、员工独立继续与检查点增量，使人员变化不迁移执行状态或篡改历史决定。

## ADDED Requirements

### Requirement: 当前委任与 authority_epoch

管理凭据 SHALL 绑定当前 Appointment、authority_epoch、scope、命令集合、时效与 audience，宿主写命令 MUST 查询当前委任与撤权状态。缓存 MUST 可失效；旧 epoch 的管理命令 MUST 被拒绝，且不得作为员工执行凭据或长期密钥下发。

#### Scenario: 旧经理提交命令
- **WHEN** 已切换 epoch 后旧经理用原管理凭据提交安排命令
- **THEN** 宿主拒绝并记录原因，不改变当前计划、预算或员工执行状态

### Requirement: 员工独立继续

负责人切换 SHALL NOT 停止已合法指派且属于其 scope 的员工执行；旧经理的合法执行 Assignment 与其管理权分别记录，安全撤权可单独停止其执行。

#### Scenario: 切换时一个任务仍在运行
- **WHEN** 经理 A 切换到 B 时，一个员工任务正在运行且另一个已接受
- **THEN** 运行任务继续、已接受成果不重做，旧管理命令被拒绝

### Requirement: 检查点增量接管

新负责人 SHALL 先核对目标、规格、验收、当前计划、在途工作、已接受成果、未知副作用与待决事项，再读取检查点后的增量并提交接管确认。初始只读确认 MUST NOT 产生管理权，随后命令仍逐次验证当前委任。

#### Scenario: 只读确认不授权
- **WHEN** 新经理完成只读现场核对但尚未提交接管确认
- **THEN** 其管理命令被拒绝；确认后仍按当前 epoch 逐条校验

### Requirement: 历史成果与费用连续

更换负责人 SHALL NOT 修改 Artifact/Revision、Manifest、Acceptance、历史费用主体或旧决定；撤任不删除旧经理的代码、报告与决定。争议记录 MUST 标待核查，不能通过换人篡改历史。

#### Scenario: 新经理要求重验旧成果
- **WHEN** 新经理要求把已接受成果全部重验
- **THEN** 系统保留旧接受并要求沿影响与重验规则提出有理由的新提案，不自动改写历史

### Requirement: 作用域边界

同一管理作用域 SHALL 只有一个当前主委任；Issue 专项委任优先于项目默认委任，嵌套专项委任由治理主体显式创建。项目负责人 MUST NOT 越过专项边界指挥其子任务，也不得通过创建任务绕过需要独立委任的专项范围。

#### Scenario: 越界指挥子任务
- **WHEN** 项目负责人在专项委任范围内创建任务以绕过边界
- **THEN** 宿主拒绝该管理命令并说明专项边界与有权主体

### Requirement: 组织元数据不迁移状态

reportsTo 等组织字段 SHALL 只是可更新的元数据，变更 MUST NOT 迁移、fencing 或保留下层任务与 Run 的执行状态；「primary agent」类用户偏好不改变 roles、权限或任务分配。

#### Scenario: 修改汇报关系
- **WHEN** 用户修改某 Agent 的 reportsTo 字段
- **THEN** 其现有任务、Run、租约与权限保持不变，不产生接管或重新派发
