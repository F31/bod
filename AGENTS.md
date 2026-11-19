# AGENTS.md — bod 仓库协作边界

> 本文件是给 Coding Agent 的硬边界声明。当前切片与禁止行为由 P0 Change 的
> `openspec/changes/p0-minimal-trusted-core/tasks.md` 驱动；修改本声明需同步该 Change。

## 当前切片

- **阶段**：P0 最小可信核心，D0 + D1 已完成（任务 1.1—1.4、2.1—2.7）。
- **已落地**：五 crate workspace（`bod-core` / `bod-app` / `bod` / `bod-desktop` / `xtask`）、
  最小 headless 启动、依赖方向守卫（`xtask verify --stage P0`）、工具链兼容记录、
  §37.14 配置 schema 与 `config explain` / `config validate`（保留来源与语义）、
  D1 领域对象/权限交集/资源状态机、SQLite WAL 初始迁移与 BEGIN IMMEDIATE 单写 UnitOfWork。
- **未落地**：队列/outbox（D2）、
  预算（D3）、执行适配（D4）、成果接受（D5）、恢复清理（D6）、桌面连接原型（8.1）。
- **唯一权威来源**：`docs/bod-V2.9.6-智能体协作开放平台完整产品与开发方案.md`；
  架构规范见 `openspec/config.yaml`。冲突时以方案文档为准并同步修订配置。

## 禁止行为

以下为硬禁止，代码审查与 `xtask verify` 均需拦截：

1. **禁止自研模型工具推理循环 / mini harness**：智能任务由外部 Agent 执行端完成，宿主只做控制面。
2. **禁止内存-only 可靠队列**：派发意图与 outbox 必须持久化，进程内通知只能加速。
3. **禁止默认外部 MQ / DB 硬依赖**：默认单宿主 + SQLite + 文件存储，外部服务按需启用。
4. **禁止第二业务写入者**：业务状态写入单一 UnitOfWork；执行端反馈不能写 Attempt/Accept 状态。
5. **禁止全权管理 Token**：权限按当前 scope/epoch 复核，不因 Profile/Skill/反馈扩权。
6. **禁止假测试报告**：planned 不等于通过；Unknown、跳过、过期报告如实记录，不把设计当证据。
7. **禁止破坏依赖方向（G-1）**：`bod-core` 不依赖 DB/Web/Tauri/Agent SDK 或上层 crate；
   `bod-app` 不反向依赖 CLI/桌面壳。
8. **禁止无证据引入重依赖**：新增默认依赖或一级对象须记录复杂度准入理由（§2.4）并锁定版本。当前新增依赖是 `serde`/`toml`（任务 1.2 配置 schema）与 `rusqlite`（任务 2.6 单写事务，bundled 避免系统 SQLite），理由与锁定版本见 `docs/spec-notes/p0-d0-config.md`、`docs/compat/toolchain.md`；`bod-core` 仍保持零外部依赖。
9. **禁止把 Fake Executor 当生产能力**：它只用于故障 conformance，不写入支持矩阵 Supported。

## 验证与完成规则

- 每个切片自带实现、测试与验证索引；不把测试路径存在当作测试通过。
- 依赖方向用 `cargo run -p xtask -- verify --stage P0` 校验，违例必须被阻断（可用 `--self-test` 证明守卫有效）。
- 编译检查：`cargo check --workspace --all-targets`，测试：`cargo test --workspace`。
- 里程碑出口：相关 I/G 门禁有真实证据后才勾选 tasks.md；归档在 P0 全部出口证据齐全后进行。
- 工具链调用前先 `export PATH="$CARGO_HOME/bin:$PATH"`（详见 `docs/compat/toolchain.md`）。
