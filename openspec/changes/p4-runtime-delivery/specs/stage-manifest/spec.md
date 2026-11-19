# Spec Delta: stage-manifest

## Purpose

定义阶段不可变清单、发布记录与证据包视图，使对外交付引用确定修订，并把包完整性、身份与业务验收分开表述。

## ADDED Requirements

### Requirement: 不可变阶段基线

Manifest SHALL 冻结一组具体 Artifact Revision、Acceptance 引用与签名引用；任何更改产生新 Manifest 修订。对外交付 MUST NOT 引用浮动最新版本，名称 MUST NOT 替代仓库 commit 或制品 digest。

#### Scenario: 修改已冻结清单
- **WHEN** 用户试图给已冻结 Manifest 增加一个制品修订
- **THEN** 系统创建新 Manifest 修订并保留旧修订，原交付引用不变

### Requirement: 修订冲突与权威来源

propose SHALL 携带 base_revision；当前接受指针已变化时 MUST 返回 Conflict 并保留候选修订。外部内容变化 SHALL 登记新修订与待复核状态，不得悄悄更新已冻结清单。

#### Scenario: 接受指针已变化
- **WHEN** 一个候选以过期 base_revision 请求接受
- **THEN** 系统返回 Conflict 并保留候选，不覆盖当前接受指针

### Requirement: 证据包视图

系统 SHALL 提供 internal、customer、audit 三种视图，分别应用字段级最小披露；任一视图 MUST NOT 包含原始密钥。包内容包括清单、摘要、授权索引、时间线、产物引用、测试证据、来源说明、复验方法与签名。

#### Scenario: 客户视图披露
- **WHEN** 客户打开证据包的 customer 视图
- **THEN** 只看到授权范围内的结论与引用，内部讨论、工具凭据与默认成本明细不可见

### Requirement: 完整性与身份分开

离线校验 SHALL 分别报告包完整性、可信签发者身份、证据关联、可执行复验与业务验收。嵌入公钥只能验证包内一致性；身份 MUST 来自预配置可信公钥、目录或证书链。自签包 MUST 显示未建立外部身份信任。

#### Scenario: 自签包通过完整性校验
- **WHEN** 一个自签证据包通过摘要校验
- **THEN** 报告仍显示外部身份未建立信任，不把完整性通过解释为业务正确

### Requirement: 重新打包身份

证据包内容清单身份 SHALL 与运输文件身份分离；ZIP/PDF 的重新打包时间戳变化 MUST NOT 改变交付内容身份。脱敏后生成独立签名包，MUST NOT 声称仍可验证被删除内容的原始连续哈希链。

#### Scenario: 重新打包
- **WHEN** 同一个证据内容被重新打包成新的 ZIP 文件
- **THEN** 内容清单 digest 不变，运输文件 digest 更新，签名覆盖明确的 payload

### Requirement: 发布记录

Publication SHALL 记录 Manifest/Revision、目标、外部标识、effect_id、状态与 observed_at；发布状态 MUST 与业务验收分开，签发者声明只有经宿主验证后才参与裁决。

#### Scenario: 外部已发布但验收未通过
- **WHEN** 连接器确认通知已发出但业务验收尚未通过
- **THEN** 系统记录发布状态为已投递、验收为未通过，不生成客户已修复的结论
