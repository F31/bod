# Tasks: P0 最小可信核心

> 规格来源：本 Change 的 `specs/*/spec.md`。实现顺序遵循《bod V2.9.6》§40.7 D0—D6 与 §40.1 P0 出口。所有 I/G 绑定初始为 planned；测试路径存在不等于通过（§39.13—39.15）。每组自带其测试与文档，不集中留到最后。

## 1. D0 基线、依赖方向与配置冻结

- [x] 1.1 建立 `bod-core`/`bod-app`/`bod`/`apps/desktop/src-tauri`/`xtask` workspace 与最小 CLI/headless 启动，验证 `cargo metadata --no-deps` 列出全部成员且 headless 可独立构建。
- [x] 1.2 实现配置 schema 与 `explain` 输出，覆盖 §37.14 的 server/desktop/sqlite/queue/cache/reliability 字段及默认值，验证未知字段被拒绝且每个默认值在 explain 中有来源与语义说明。
- [x] 1.3 实现依赖方向守卫，保证 `bod-core` 不依赖存储/网络/app、`bod-app` 不反向依赖 CLI/桌面，验证 `cargo run -p xtask -- verify --stage P0` 报告 G-1 通过且违例被阻断。
- [x] 1.4 编写 `AGENTS.md` 当前切片与禁止行为（无自研模型循环、无内存-only 可靠队列、无默认外部 MQ、无第二业务写入者、无全权管理 Token、无假测试报告），验证 CI 或 xtask 能读取并校验该声明存在。

## 2. D1 领域对象、权限与单写事务

- [x] 2.1 实现默认 Company、Project、Agent、Issue 的最小记录与直接指派（identity-access: 默认 Company 与归属、直接指派），验证 `bod-app::authorization` 中无 Team 配置可完成指派且 Coding Issue 缺项目/仓库上下文时被拒绝。
- [x] 2.2 实现权限交集与当前授权复核（identity-access: 权限交集与当前复核、执行反馈与主体身份分离），验证 `invariant_i_09`、`invariant_i_12` 覆盖撤权后拒绝、角色称号与 payload 自报不扩权。
- [x] 2.3 实现稳定拒绝原因码、审计字段与本地监听边界（identity-access: 拒绝可解释与审计、本地部署边界），验证每个拒绝含 actor/scope/修订/重新激活条件，且默认配置只绑定本地地址。
- [x] 2.4 实现资源外壳与状态机（resource-state: 资源身份与条件更新、期望与执行约定分离、条件与 phase 各司其职），验证 `bod-app::resource_state` 的 `invariant_i_27` 覆盖版本冲突、观察不改 generation、运行中拒绝改验收标准。
- [x] 2.5 实现状态写入所有权与稳定停止状态（resource-state: 状态写入所有权、正确停止状态），验证执行端自报不能写 Attempt 状态、AwaitingApproval/InsufficientBudget/ExternalOutcomeUnknown 不产生热重试、Succeeded 不重跑。
- [x] 2.6 实现初始 SQLite 迁移与 UnitOfWork（events、issues、work_package_revisions、runs、attempts、leases、members、grants、config_revisions），使用 WAL、BEGIN IMMEDIATE 与版本号条件更新，验证 `cargo run -p xtask -- migration-check` 通过且并发写测试中只有一个条件更新成功。
- [x] 2.7 记录 D1 的领域状态迁移与权限矩阵到 `docs/spec-notes/p0-resource-state.md`，验证文档中每个枚举与写入者都能追溯到本 Change 的对应 requirement 与测试 case。

## 3. D2 唤醒回执、资源队列与 outbox

- [x] 3.1 实现 wake_receipts 逻辑键去重与保留窗口（coordination-loop: 唤醒回执去重），验证 `bod-app::wakeup` 的 `invariant_i_68`、`invariant_i_69` 覆盖重复命令返回原回执、同键异参拒绝、定时 occurrence 不伪装重试。
- [x] 3.2 实现 work_queue 的 wanted/claimed/completed_generation 与运行中新增不丢失（coordination-loop: 运行中新工作不丢失），验证 `invariant_i_70` 在执行中注入新唤醒后仍处理新增代数且不创建第二个消费者。
- [x] 3.3 实现原子认领、lease_token 与 fencing（coordination-loop: 原子认领与租约；effect-gate: 取消确认与 fencing），验证 `bod-app::claim_concurrency` 的 `invariant_i_01`—`invariant_i_03` 与 G-2：并发认领唯一成功、旧 token 被拒绝、同逻辑键重试返回原租约。
- [x] 3.4 实现提交后 outbox 派发与重启恢复（coordination-loop: 重启恢复；effect-gate: 提交后派发），验证 `bod-app::effect_recovery` 的 `invariant_i_04`、`invariant_i_11` 覆盖提交后崩溃、重放不触发副作用、先 inspect/reconcile 再派发。
- [x] 3.5 实现有界退避、稳定 Blocked 与 dead 状态及队列观测指标（coordination-loop: 稳定等待与有界退避、队列可观测），验证故障注入中权限/预算阻塞无热重试、超预算转 dead 且 doctor 能列出队列深度与最老未处理时间。

## 4. D3 预算准入与不可变账本

- [x] 4.1 实现固定次序的分层准入事务（Company→Project→Agent→Run→WorkPackage→槽位→session/workspace→Reservation/Binding/派发意图）（budget-admission: 分层准入事务），验证 `bod-app::budget_admission` 的 `invariant_i_05` 在并发申请最后额度时只有一个成功且失败方无残留 Binding。
- [x] 4.2 实现 C/R/U 互斥记账与取消/过期转换（budget-admission: 确认、预留与风险互斥），验证 `invariant_i_10` 的同类场景：同一金额不同时计入 R 与 U，取消转风险占用一减一增且总量不变。
- [x] 4.3 实现不可变账本分录、外部 usage_key 唯一与超额调整（budget-admission: 超额如实入账），验证 `invariant_i_06`：迟到且高于预留的账单如实入账、超额被记录且阻止后续派发，不出现截断 actual 的假平衡。
- [x] 4.4 实现多维约束、定点金额与 Busy 有界重试（budget-admission: 多维约束与计量单位、Busy 与稳定 request_id），验证不同模型 token 不相加为现金、跨维度任一触限即拒绝、Busy 超时保留稳定 request_id 且整体重读重试。
- [x] 4.5 实现默认无隐式付费授权与预算解释（budget-admission: 默认无隐式付费授权、预算解释与拒绝原因），验证首次启动无默认消费额度、观察/免费模式明确标识、拒绝解释列出 C/R/U 与来源且未知不显示为可用。
- [x] 4.6 编写 D3 预算准入顺序与错误码说明到 `docs/spec-notes/p0-budget.md`，验证其中每条原因码与本 Change 的 scenario 及 `bod-app::budget_admission` 用例一一对应。

## 5. D4 执行端协议与一个真实适配器

- [x] 5.1 实现 `bod-execution/v1` 最小契约与 schema（describe、probe、start、inspect、observe、分项能力声明），验证 `bod-app::descriptor_schema` 与 G-5：schema fixture 覆盖全部必需操作且未知能力标 unsupported/unknown。
- [x] 5.2 实现 invocation 持久去重、同 ID 异参拒绝与回包丢失先对账（execution-adapter: Start 请求与持久去重、回包丢失先对账），验证 `invariant_i_04` 与 G-17：重复 start 返回原 execution_id、无幂等能力时进入 Unknown 且不新建 Attempt。
- [x] 5.3 实现执行状态映射与观察信封边界（execution-adapter: 接收状态与终态边界、观察信封与缺口），验证 Completed 不等于 Accepted、退出码 0 不触发接受、乱序观察不越过缺口 ACK、ACK 不表示业务完成。
- [x] 5.4 实现 Fake Executor 与最小 conformance 样本集（重复 start、同 ID 参数变化、开始回执丢失、乱序/断档、进程退出、取消请求与确认分离、认证失效、超大日志、最终费用迟到），验证 `bod-app::adapter_conformance` 的 G-17 列出全部命名用例与真实断言。
- [x] 5.5 接入一种本机已就绪的真实执行配置，完成 describe/probe/start/inspect/observe 与分项能力声明（execution-adapter: 必需操作与分项探测、能力分项声明），验证真实 conformance 报告注明运行时版本与探测结果，不支持的 cancel/ACK/配额能力在界面显示限制且不写入支持矩阵 Supported。
- [x] 5.6 记录执行端能力覆盖声明与证据等级到 `docs/spec-notes/p0-adapter-matrix.md`，验证每项声明含来源与观测时间，等级不高于证据来源且 Fake Executor 未被列为生产能力。

## 6. D5 成果修订、最小接受与控制 API

- [x] 6.1 实现 Artifact/Revision 与 CAS 临时文件→digest→原子转正→事务登记（artifact-acceptance: 不可变成果修订），验证 `bod-app::artifact_acceptance` 的 `invariant_i_07`：崩溃孤儿不被当有效成果、外部仓库对象不被 GC 删除。
- [x] 6.2 实现验证状态机与 Waived 语义（artifact-acceptance: 候选与验收分离），验证执行 Completed 后验收为 Pending、Waived 记录主体/理由/版本且不显示「测试已通过」。
- [x] 6.3 实现 Acceptance 绑定输入基线、工作包修订、产物修订、验证规则与目标环境，并拒绝实现者自批（artifact-acceptance: 接受绑定确定修订、不可隐式自批），验证 `invariant_i_08`、`invariant_i_51` 与 G-9：旧接受不能批准新产物、自报通过不改变接受主体。
- [x] 6.4 实现 Issue 多工作包完成条件与修订失败回路（artifact-acceptance: 多工作包完成条件、决策记录），验证部分工作包接受不完成 Issue、验收失败形成 NeedsRevision 与新 Attempt 且不覆盖历史约定。
- [x] 6.5 实现控制 API 的最小资源/命令入口、稳定错误码与 409 冲突语义，并保证只读查询不启动 Agent（resource-state: 资源身份与条件更新；identity-access: 执行反馈与主体身份分离），验证 `bod-app::authorization` 的 G-21：越权 403、版本冲突 409、只读请求零派发。
- [x] 6.6 用 Fake Executor 跑通一次从 Issue 指派到候选结果登记的最小 CLI 冒烟，验证全流程可在宿主重启后按 invocation 找回且没有把候选结果误标为 Accepted。

## 7. D6 恢复、doctor、清理与 P0 故障矩阵

- [x] 7.1 实现一致性备份（DB+WAL+CAS 引用+配置/适配版本+密钥恢复说明）与新宿主恢复流程（recovery-cleanup: 备份内容、恢复到新宿主），验证 `bod-app::migration_safety` 的 G-6/G-49：缺 WAL 的备份不标可恢复、恢复后在途 Effect 先对账才开放派发。
- [x] 7.2 实现 doctor 与 ReconciliationCase（recovery-cleanup: 最小 doctor、对账案件），验证 `invariant_i_26` 与 G-20：磁盘满停止新派发、Unknown 保留风险占用且不被 TTL 回收、每个案件有处理人与下一步。
- [x] 7.3 实现删除请求、CleanupBlocked、Abandon 与外部共享资源边界（recovery-cleanup: 删除请求与清理阻塞、外部共享资源边界；resource-state: 删除请求与清理边界），验证 `invariant_i_55`：停止未确认/费用未结时阻塞清理、外部仓库与 Formal Manifest 引用不随项目删除消失、Abandon 记录残留。
- [x] 7.4 迁移元数据含 migration_id/checksum/依赖/兼容窗口/恢复说明，破坏性 DDL 默认阻断（recovery-cleanup: 迁移与恢复测试），验证 `cargo run -p xtask -- migration-check` 与旧库样本恢复测试通过、已发布脚本不可原地改写。
- [x] 7.5 执行 P0 核心故障矩阵：外部成功但回包丢失、已预留后宿主崩溃、磁盘满、权限被撤销、备份恢复到新宿主，验证每个故障留下事实记录与可操作下一步，报告注明实际环境、commit 与断言摘要。
- [x] 7.6 实测空闲负担与 §38 性能预算项（宿主空闲 CPU/内存、队列扫描成本、写线程等待），验证报告给出测量方法与环境；未测项保留 Target/Unverified，不以设计值代替实测。

## 8. 桌面连接原型、验证索引与发布纪律

- [ ] 8.1 实现 Tauri 2 壳的 connect_or_spawn、握手取地址与 close_action=detach 原型（不启动第二个业务 DB），验证桌面退出不终止外部已有宿主、启动密钥不出现在日志。
  - 进度：宿主对端与连接控制器已完成并端到端验证——`bod serve`（受保护握手/健康检查/授权停止/原子描述文件）+ `bod-desktop::connection` 的 Std 实现；`cargo test -p bod-app host_handshake` 6 项、`cargo test -p bod --test host_handshake_e2e` 2 项（真实子进程）、`cargo test -p bod-desktop` 7 项全过。
  - 未完成：Tauri 2 窗口本体。加入 `tauri = "2"` 后本机 cargo 在 435 包依赖图上持续停滞（20 分钟 0 行输出、无 rustc 子进程；去掉该依赖后 33 秒完成 check），已回退依赖以保证工作区可构建。恢复步骤与实测数据见 `docs/spec-notes/p0-desktop.md` §2，故不勾选。
- [x] 8.2 运行前端检查链 `npm --prefix apps/workbench ci`、`run typecheck`、`run test`、`run build`，验证四项全部通过且桌面/headless 结果分别记录，不用 headless 结果宣称桌面通过。
  - 证据：新建 `apps/workbench`（TypeScript + `node --test`，依赖仅 typescript/@types/node）。`npm --prefix apps/workbench ci` → added 3 packages；`run typecheck` 通过；`run test` → 8/8；`run build` → `dist/{index.html,main.js,connection.js}`。桌面侧未实测，未宣称桌面通过（见 `docs/spec-notes/p0-desktop.md` §3.3）。
- [x] 8.3 汇总本 Change 的 I/G 验证索引并运行 `cargo run -p xtask -- verify --stage P0`、`cargo fmt --all -- --check`、`cargo clippy -p bod-core -p bod-app -p bod --all-targets -- -D warnings`、`cargo test -p bod-core`、`cargo test -p bod-app`，验证 G-50 报告列出全部绑定的 planned/implemented 状态且无未知或重复 I/G 引用。
- [x] 8.4 编写 P0 支持矩阵与已知限制，标明第二执行端、接续、委任切换、跨模块影响图、远程与移动均为 Planned/未支持，验证矩阵中每个 Supported 项有对应 conformance 或真实测试报告，Fake Executor 与 planned 路径不计入。

## Workflow follow-up

- 在评审确认全部 P0 出口证据后归档本 Change，并由单一归档入口把 8 份 spec delta 同步到 `openspec/specs/`。
- 归档后验证现行规格与 tasks.md 的需求覆盖一致；P1—P7 以新的 Change 承接，不修改本 Change 的历史约定。
