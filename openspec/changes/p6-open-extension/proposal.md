# Proposal: P6 按需开放

## Why

P0—P5 覆盖了本地单宿主到团队复用，但第三方运行时、远程节点、只读查询和移动入口有独立的信任与恢复风险。没有能力协商、设备身份、签名信封、分区与撤权边界时，开放接口会把本地确定性边界暴露给不可信节点，或把「协议已定义」误当成支持（§14、§15.11、§20、§31.13—31.16）。

## What Changes

- 提供 SDK/第三方适配接入：描述符、能力握手、分项 conformance 与安装边界；第三方不改宿主安装即可接入，不支持的能力在调度与界面一致显示。
- 开放经验证的协议子集与远程委派：节点/webhook 共用 Start 结构、按能力握手协商版本、签名事件批次、持久游标、分区与设备撤销；HTTP/SSE 为基线，QUIC/隧道/中继按实测决定。
- 提供只读查询适配：列表/详情/导出/订阅在返回前过滤，只读请求不改变控制状态或启动 Agent，审计与有界计算允许。
- 提供移动伴随入口与 H5 回执：设备密钥签名审批、离线投递期限与容量、到期/撤权拒绝；六段流程按阶段与真机证据开放，不宣称手机完成全流程除非链路与 MD 证据齐备。
- 非目标：容量引擎迁移（P7）、在线市场、原生 ICE 与多宿主 MQ、受监管行业定制。

## Capabilities

### New Capabilities

- `sdk-extension`: 第三方适配器/SDK 描述符、能力握手、符合性准入与不修改宿主的扩展边界。
- `remote-execution`: 节点/webhook 委派、签名信封、持久游标、分区、撤权、停止与恢复。
- `readonly-adapter`: 只读查询与订阅适配、返回前过滤、无业务副作用与审计边界。
- `mobile-control`: 移动审批/H5 回执、设备签名、离线投递与六段交付分期。

### Modified Capabilities

无。P0—P5 Change 尚未归档；本 Change 以新 capability 路径建立 P6 行为基线，不改写既有历史约定。

## Impact

- 代码：`crates/bod-app/src/extensions/`、`crates/bod-app/src/remote/`、`crates/bod-app/src/query/`、`apps/mobile`/H5（按分期）、`xtask`/`bod conformance`。
- 数据：新增 adapter_descriptors、node_registrations、device_grants、event_batches、cursors、mailbox_messages、readonly_bindings、mobile_signatures。
- 验证：绑定 I-15、I-40、I-50、I-55、RM01—RM09、MD01—MD04、SB05—SB06、G-19、G-23、G-34、G-46—G-47（§39.13—39.14、§39.20—39.23）；初始 planned。
- 依赖：P2 符合性工具、P4 回执、P5 权限矩阵必须可核对；远程专项在本地 Start/验收/恢复跑通后进入，不给单机包增加部署前提。
