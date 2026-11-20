# Proposal: P5 复用与团队

## Why

P4 能把一次交付做到客户可核对，但重复配置与多人协作仍依赖人工：团队希望复用一套 Agent/Profile/Skill 配置，成员、客户和执行者在不同阶段拥有不同权限，验收反馈要能沉淀为可撤销的项目约定，并在离开桌面后通过一个通道处理待决定事项（§6、§22—23、§30、§31.15）。

## What Changes

- 引入 Team Catalog 作为协作配置包目录：确定版本安装、来源/完整性/许可证校验、三方升级比较、幂等卸载；安装结果不构成权限，也不覆盖用户修改或交付成果。
- 实现六阶段交付权限矩阵：提交、范围分派、开发、测试验收、发布回滚、证明确认分别校验当前授权与资源范围，按实际主体拒绝并记录审计。
- 实现受控反馈学习与知识晋升：反馈分类保留最窄作用域，候选修订经冲突检查与授权接受后才生效；权限、预算、生产发布不能通过学习管道改变，支持一键撤销与新会话生效。
- 接入一个通知/IM 通道：三类卡片、受限动作、签名命令与投递回执；撤权、离线修订过时或渠道回包丢失时如实显示，不把接收当业务成功。
- 非目标：第三方 SDK 与公开适配市场、远程节点/webhook、移动原生 App 与 H5 全链路、容量引擎迁移。

## Capabilities

### New Capabilities

- `team-catalog`: 配置包目录、安装/升级/卸载事务、冲突保留与安装记录。
- `team-permissions`: 六阶段业务动作授权矩阵、资源范围校验、分离审计与拒绝语义。
- `knowledge-promotion`: 反馈分类、候选知识/Profile/Skill 修订、作用域、接受/撤销与效果记录。
- `notification-channel`: 一个通道的卡片投影、受限命令、投递回执与更正。

### Modified Capabilities

无。截至 P1 归档（2026-11-20），现行 `openspec/specs/` 含 13 个 capability（P0 9 + P1 4）；本 Change 的能力与之均不同名，全部以 ADDED 新增，不改写既有规格与历史约定。

## Impact

- 代码：`crates/bod-app/src/catalog/`、`crates/bod-app/src/governance/`、`crates/bod-app/src/feedback/knowledge/`、`crates/bod-app/src/connectors/`。
- 数据：新增 catalog_install_records、project_access_grants、feedback_proposals、knowledge_revisions、preference_revisions、channel_bindings、delivery_receipts。
- 验证：绑定 I-14、I-19、I-23、I-44、I-55、AZ01—AZ06、G-13、G-21—G-22、G-25（§39.10、§39.13—39.14、§39.22）；初始 planned。
- 依赖：P1 讨论、P3 影响图、P4 回执与证据包必须可用；不新增外部 MQ 或默认向量检索。
