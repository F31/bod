# AGENTS.md — bod 仓库协作边界

> 本文件是给 Coding Agent 的硬边界声明。当前切片与禁止行为由 P0 Change 的
> `openspec/changes/p0-minimal-trusted-core/tasks.md` 驱动；修改本声明需同步该 Change。

## 当前切片

- **阶段**：P0 最小可信核心，D0—D6 已完成、D7 部分完成（任务 1.1—1.4、2.1—2.7、3.1—3.5、4.1—4.6、5.1—5.6、6.1—6.6、7.1—7.6、8.2—8.4；8.1 缺 Tauri 窗口本体）。
- **已落地**：五 crate workspace（`bod-core` / `bod-app` / `bod` / `bod-desktop` / `xtask`）、
  最小 headless 启动、依赖方向守卫（`xtask verify --stage P0`）、工具链兼容记录、
  §37.14 配置 schema 与 `config explain` / `config validate`（保留来源与语义）、
  D1 领域对象/权限交集/资源状态机、SQLite WAL 初始迁移与 BEGIN IMMEDIATE 单写 UnitOfWork、
  D2 持久 wake_receipts/work_queue/outbox（唤醒去重、代数合并、原子认领/fencing、
  提交后派发与重启对账、有界退避/dead/doctor 指标），UnitOfWork 错误路径自动回滚、
  D3 预算准入与不可变账本（五层固定次序、C/R/U 互斥、usage_key 去重、超额如实入账、
  默认无隐式付费授权、预算解释、Busy 整事务重试）、
  D4 执行协议（bod-execution/v1 描述符/Start/观察 schema、先落盘再发送的 invocation 去重、
  回包丢失转 Unknown 不新建 Attempt、终结态边界与 Completed≠Accepted、观察缺口 ACK 与快照、
  Fake Executor 故障 conformance 10 例、本机真实进程适配器 describe/probe/start/inspect/observe/cancel）、
  D5 成果与接受（SHA-256 内容寻址 CAS「临时文件→digest→原子转正→事务登记」、孤儿 GC dry-run
  与外部对象保护、不可变修订 trigger、验证状态机与 Waived≠通过、接受绑定五元组与指针 CAS、
  实现者自批拒绝、多工作包完成条件、失败转 NeedsRevision 新 Attempt、控制 API 403/409 稳定码、
  只读零派发、`bod smoke` 全流程冒烟）、
  D6 恢复与清理（迁移登记与 checksum 漂移检测、`VACUUM INTO` 一致性备份与不可恢复判定、
  新宿主恢复与派发门禁、doctor 9 项检查与 ReconciliationCase 幂等开案、删除请求与四项清理条件、
  CleanupBlocked 稳定阻塞码、Abandon 残留清单、外部仓库/正式 Manifest/接受交付永不自动销毁、
  清理 Effect Gate 幂等、`bod fault-matrix` 5 场景与 `bod perf` 实测）、
  D7 收口（`bod-desktop::connection` 的 connect_or_spawn/握手/detach 控制器、真实
  `StdHostProbe`/`StdHostSpawner`/`StdHostControl` 与 7 项测试）、
  `verification/gates.yaml` + `bod verify-index` + `xtask verify` 的 G-50 覆盖检查、
  P0 支持矩阵与已知限制、
  宿主控制面 `bod serve`（受保护握手/健康检查/授权停止，6 项单测 + 2 项真实子进程 e2e）、
  `apps/workbench` 前端工作台（TS + `node --test`，ci/typecheck/test/build 四项实测通过）。
- **未落地**：Tauri 2 窗口本体（加入 tauri 依赖后本机 cargo 在 435 包图上持续停滞，已回退；
  实测数据与恢复步骤见 `docs/spec-notes/p0-desktop.md` §2）。
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
8. **禁止无证据引入重依赖**：新增默认依赖或一级对象须记录复杂度准入理由（§2.4）并锁定版本。当前新增依赖是 `serde`/`toml`（任务 1.2 配置 schema）、`rusqlite`（任务 2.6 单写事务，bundled 避免系统 SQLite）与 `serde_json`（任务 5.1 执行协议 JSON 契约，需未知字段拒绝），理由与锁定版本见 `docs/spec-notes/p0-d0-config.md`、`docs/spec-notes/p0-execution-protocol.md`、`docs/compat/toolchain.md`；`bod-core` 仍保持零外部依赖。
9. **禁止把 Fake Executor 当生产能力**：它只用于故障 conformance，不写入支持矩阵 Supported。

## 验证与完成规则

- 每个切片自带实现、测试与验证索引；不把测试路径存在当作测试通过。
- 依赖方向用 `cargo run -p xtask -- verify --stage P0` 校验，违例必须被阻断（可用 `--self-test` 证明守卫有效）。
- 编译检查：`cargo check --workspace --all-targets`，测试：`cargo test --workspace`。
- 里程碑出口：相关 I/G 门禁有真实证据后才勾选 tasks.md；归档在 P0 全部出口证据齐全后进行。
- 工具链调用前先 `export PATH="$CARGO_HOME/bin:$PATH"`（详见 `docs/compat/toolchain.md`）。
