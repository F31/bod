# Spec Delta: configuration

## Purpose

把 V2.9.6 §37.9/§37.14 的部署配置集中为可校验的 schema，使所有默认值先有定义和来源再可被使用，并通过 explain 显示实际生效值、来源与语义，避免把未实测初值伪装成容量承诺或把配置散落进适配器分支。

## ADDED Requirements

### Requirement: 集中配置 schema 与默认值来源

宿主 SHALL 在配置 schema 中定义 §37.14 的 `server`/`desktop`/`sqlite`/`queue`/`cache`/`reliability` 全部字段、类型与默认值；默认值 MUST 标注来源为「§37.14 建议初值、D0 未冻结、待故障/负载测试」，MUST NOT 被表述为实测容量承诺。字段 MUST NOT 在 schema 之外被适配器分支隐式使用。

#### Scenario: 首次启动只使用已定义默认值
- **WHEN** 用户不提供任何配置文件启动宿主
- **THEN** 系统按 schema 使用 §37.14 建议初值，且每个值都能在 explain 中查到类型、默认值、来源与语义

### Requirement: 未知字段与非法值被拒绝

配置解析 SHALL 在合并前检查片段、合并后检查完整 schema；未知 section 或未知字段 MUST 被拒绝并给出完整路径，非法枚举值 MUST 被拒绝。静默忽略未知键或把拼写错误当默认值 SHALL NOT 发生。

#### Scenario: 未知字段被拒绝
- **WHEN** 配置文件写入 `[server]` 下的 `auth_require`（正确字段为 `auth_required`）
- **THEN** 校验失败并报告路径 `server.auth_require`，宿主不以默认值继续启动

#### Scenario: 非法枚举值被拒绝
- **WHEN** 配置文件把 `desktop.framework` 写成未定义的值
- **THEN** 校验失败并说明该字段允许的取值，不静默回退到 `tauri2`

### Requirement: 部分覆盖与来源追踪

标量字段 SHALL 逐键覆盖；未出现的字段 MUST 保留 schema 默认值。explain MUST 区分「内置默认」与「文件覆盖」，并在覆盖时同时显示被替换的默认值。

#### Scenario: 部分文件覆盖
- **WHEN** 配置文件只设置 `[server] listen = "127.0.0.1:8080"`
- **THEN** 其余字段仍为 §37.14 默认值，explain 显示 `server.listen` 来自文件覆盖、原默认 `127.0.0.1:0`

### Requirement: 配置 explain 的语义与边界

`explain` SHALL 对每个字段展示生效值、类型、默认值、来源与语义说明；语义 MUST 明确 `listen` 端口 0、`claim_lease_seconds` 与 Agent 执行超时的区别、`minimum_retry_window_hours` 与 Unknown 去重保留的关系、缓存上限非预分配。explain MUST NOT 输出启动密钥或完整 prompt。

#### Scenario: 解释租约与重试窗口
- **WHEN** 用户查看 `queue.claim_lease_seconds` 与 `reliability.minimum_retry_window_hours` 的 explain
- **THEN** 前者说明它是协调消费者租约而非 30 秒 Agent 执行超时，后者说明 168 小时只是下界且到期不删除 Unknown 去重
