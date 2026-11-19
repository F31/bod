# Spec Delta: team-catalog

## Purpose

定义可安装协作配置包的目录与生命周期，使团队能复用 Agent/Profile/Skill/模板，而不把安装结果当权限或覆盖本地成果。

## ADDED Requirements

### Requirement: 安装事务

安装 SHALL 选择确定包版本、校验来源/完整性/许可证、预览拟创建与复用对象、检测名称/ID/版本冲突、选择运行时与凭据引用、明确授权，并在事务内记录安装计划与结果。联网下载 MUST NOT 占用数据库写事务，候选资源正式启用前 MUST NOT 执行。

#### Scenario: 安装新包
- **WHEN** 用户安装一个确定版本的协作配置包
- **THEN** 系统先展示对象预览与冲突清单，授权后事务记录安装结果，下载期间不持有写锁

### Requirement: 安装记录与幂等

CatalogInstallRecord SHALL 记录包来源、digest、schema 版本、目标 Company、对象 ID 映射、安装参数摘要与已应用修订。重复请求 MUST 用安装逻辑键去重，MUST NOT 仅按名称覆盖已有 Agent、项目或规则。

#### Scenario: 重复安装请求
- **WHEN** 同一安装逻辑键与相同参数被重复提交
- **THEN** 系统返回原安装结果，不创建第二份配置或修改已有对象

### Requirement: 三方升级

升级 SHALL 比较原包基线、当前本地修订与新包版本；用户修改过的 Profile/Skill 冲突时 MUST 保留本地并生成提案，不静默覆盖。新增权限、外部程序与付费连接 MUST 重新准入，安装包 MUST NOT 携带真实密钥或自动取得管理员权。

#### Scenario: 本地修改与升级冲突
- **WHEN** 用户修改过的 Skill 与新包版本冲突
- **THEN** 系统保留本地修订并生成升级提案，不覆盖用户修改

### Requirement: 卸载边界

卸载 SHALL 默认解除安装追踪或停用包独占且未使用的配置；Issue、交付、用量与历史证据 MUST 保留。被其他项目、任务或安装包引用的资源 MUST NOT 级联删除，应列出影响并按明确策略处理。

#### Scenario: 卸载被引用的包
- **WHEN** 一个配置包中的 Agent 仍被已接受 Issue 引用
- **THEN** 卸载列出影响并保留引用对象与历史证据，不删除交付成果

### Requirement: 目录与权限分离

能力目录回答「当前谁适合这项工作」，Team Catalog 回答「怎样安装一套协作配置」；两者可共用入口，数据身份 MUST NOT 混合。目录推荐、组织图与 reportsTo 元数据 MUST NOT 授予审批、预算或资源权限，退出导航不撤权。

#### Scenario: 推荐即授权
- **WHEN** 目录把一个 Agent 推荐为某任务首选
- **THEN** 宿主仍按当前 Grant 与 Assignment 准入，不因推荐扩大权限
