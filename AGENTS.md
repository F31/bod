# AGENTS.md — bod 仓库协作边界

> 本文件是给 Coding Agent 的硬边界声明。P0 已归档（2026-11-19），P1 已归档（2026-11-20），
> 现行能力规格在 `openspec/specs/`（13 个 capability）；历史约定分别见
> `openspec/changes/archive/2026-11-19-p0-minimal-trusted-core/tasks.md` 与
> `openspec/changes/archive/2026-11-20-p1-single-executor-delivery/tasks.md`；
> 后续切片由 `openspec/changes/` 下对应 Change 驱动。修改本声明需同步现行规格。

## 当前切片

- **阶段**：P0 最小可信核心，D0—D7 全部完成并**已归档**（任务 1.1—1.4、2.1—2.7、3.1—3.5、4.1—4.6、5.1—5.6、6.1—6.6、7.1—7.6、8.1—8.4；44/44 勾选，`openspec archive` 于 2026-11-19 将 9 份 spec delta 的 57 条需求、66 个场景同步进 `openspec/specs/`）。P1 单执行者交付全部完成并**已归档**（1.x—5.x 全部勾选，`openspec archive` 于 2026-11-20 将 4 份 delta 的 21 条需求、21 个场景同步进 `openspec/specs/`；现行合计 13 个 capability / 78 条需求 / 87 个场景）。
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
  `StdHostProbe`/`StdHostSpawner`/`StdHostControl`，7 项单测 + 2 项真实子进程 `detach_e2e`；
  独立 crate `apps/desktop/shell` 的 Tauri 2 窗口本体，真实窗口关窗实测两种宿主场景）、
  `verification/gates.yaml` + `bod verify-index` + `xtask verify` 的 G-50 覆盖检查、
  P0 支持矩阵与已知限制、
  宿主控制面 `bod serve`（受保护握手/健康检查/授权停止，6 项单测 + 2 项真实子进程 e2e）、
  `apps/workbench` 前端工作台（TS + `node --test`，ci/typecheck/test/build 四项实测通过）。
- **已知限制**（P0 归档时如实保留，未作为 Supported 承诺）：桌面壳 `bundle.active=false` 未打安装包、
  图标为占位、未断言页面渲染内容；Tauri 窗口只覆盖关窗与进程退出，未覆盖系统关机/崩溃路径；
  前端证据仅 headless 与窗口启动，不含页面 DOM 断言。详见 `docs/spec-notes/p0-desktop.md` §2.4 与
  `docs/spec-notes/p0-support-matrix.md`。
- **当前推进**：P1 单执行者交付，已完成切片 1「工作区与 Git 基线」（任务 1.1—1.5）与切片 2「Context 与 Profile」（任务 2.1—2.5）——
  `bod-app::workspace_safety` 提供每 Attempt 独立 worktree/分支、`write_scope` diff 门禁、
  共享资源（migration/API/公共类型等）登记与合并门禁、脏现场保留登记、端口与目录隔离（G-29 implemented）；
  `bod-app::context_handoff` 提供确定装配的 ContextBundle（I-17）、按 revision/digest 的不可变片段缓存与撤权复核、
  指令优先级与 Question 归口、会话复用与角色/Profile/授权重置（I-36、G-11 implemented；G-48 因容量与热点回源未实现而登记 `unverified`）。
  **本切片内存值类型 + 只读装配，未落 `workspaces`/`context_bundles` 表**，落库留到 4.x 直派闭环。
  已完成切片 3「讨论与澄清」（任务 3.1—3.4）——`bod-app::clarity_dialogue` 提供带编辑历史的讨论记录、
  事件→控制命令的唯一映射（评论/不可信材料恒零命令）、Question/NeedsInput 三态与超时保持 pending、
  评论→Decision/ScopeDecision/FeedbackProposal 的显式转换（四检 + `proposed`）、外部回调「认证→主体→
  对象→修订→授权」顺序（伪造批准只登记不可信材料）；I-38 登记 implemented，同样未落表。
  已完成切片 4.2「真实仓库直派闭环」——`bod-app::coding_integration` 提供共享指令/规则按确定
  base 串行修订（I-18/I-20）、真实 git 提交候选绑定、本地验证报告的独立性检查与 NeedsRevalidation、
  人工/预授权接受（复用 P0 `artifact_acceptance`）以及重启后从同一 SQLite 文件查询交付快照；
  沿用 P0 表，不建第二业务库。已完成切片 4.3「取消与 Unknown 对账」——`bod-app::cancel_reconciliation`
  把取消请求与停止确认分离：cancel_ack 只记送达，不释放预留、不新建 Attempt、不结案；Unknown 显示
  待核实与处理人，确认停止需独立证据；`pending_cancellations` 只投影宿主事件与案件（I-11 新用例）。
  已完成切片 4.1「首次使用路径」——`bod-app::first_use` 只需 Project + 已接入 Agent + 真实仓库即可
  自动补齐 Issue/工作包修订/Run/Attempt 与最小写范围并登记 git 候选，不要求 Team/组织图/Run 预配置；
  I-10 按 §39.2 权威语义落在「合并证据对应实际目标与候选组合」：`merge_combination` 绑定目标摘要、
  提交、规则与环境，跨目标/自报证据不得合并，接受后重启可查。tasks.md 曾把 I-10 误绑到「无 Team 配置」
  与 C/R/U 互斥，前者已纠正为宿主入口，后者仍归 I-71。
  已完成切片 4.4「本地验证报告绑定与备份核验」——`bod-app::delivery_verification` 真实执行 argv
  命令（有界超时 + 输出摘要），报告正文落 CAS 并从 `verification_records` 反查证据，自报/跨提交/
  缺正文均不接受；`verify_backup_for_delivery` 在 P0 `verify_backup` 之上补交付 CAS 引用覆盖，
  缺 WAL/缺 CAS/快照被改均不标可恢复（G-6/G-9 的 P1 侧断言，module/tests 仍保持 P0 目标）。
  已完成切片 4.5「工作台只读投影」——`bod-app::workbench_projection` 以纯函数把宿主事实快照投影成
  首页四问（目标/下一项交付/受阻/待用户决定）与任务页证据视图；每张待决定卡强制带阻塞主体、
  所需决定与可采取动作，Unknown 不猜"已停止"，任务页证据按修订→成果关联归属并显示拒绝原因；
  `collect_host_facts` 只用 `SqliteStore` 只读 `list_*`、不写库不派发，相同事实得相同视图（I-55 的
  P1 侧断言，module/tests 仍保持 P0 control_api 目标）。
  已完成切片 4.6「预算耗尽派发门禁」——`bod-app::budget_dispatch_gate` 只读复核派发入口可确定的
  Company/Project 显式限额（固定次序、未配置不阻塞），`control_api::request_dispatch` 在耗尽或已超额时
  同事务写 `Blocked + InsufficientBudget + 解除条件`，返回稳定 409 `BOD_INSUFFICIENT_BUDGET`，
  不创建 Reservation/Outbox/Attempt/Binding、不触碰派发探针；工作台可见 `approve_budget` 入口与具体解除条件，
  提额后显式重派解除阻塞，已接受成果与已结算费用全程只读保留（G-45 的 P1 并列断言）。
  入口只判 CNY 金额维度的 1 微元下界，真实请求量仍由 P0 `budget_admission::admit_in` 五层复核；
  未新增提额命令，UI 动作仍只展示。
  已完成切片 4.7「首次使用与交付操作手册」——`docs/spec-notes/p1-user-flow.md` 的 8 个步骤由
  `tests/p1_user_flow_e2e.rs` 在真实 git/SQLite/CAS 上逐步投影到首页与任务页核对；修复首次使用
  把 `project_id` 当 `company_id` 写入验证/接受的缺陷，并用只读 `issue_work_package_links` 把
  `delivery-*` 证据归属到 `issue-*` 任务页、结案案件不再冒充待决定。Unknown/取消按 ack≠停止、
  风险预留保留、独立证据才确认停止诚实表达；真实 Agent SDK/UI 点击/桌面进程重启未复现并已列明。
  P1 出口切片（5.1—5.3）已完成：G-15/G-16 补命名用例，G-51 按非目标保持 planned，
  I-71 独立登记；`verification_registry.rs` 的 G-50 绑定来源保留 P0 归档并追加 P1
  proposal/tasks（44 条、缺失引用 0）。e2e 补报告正文 commit/环境断言与备份快照重开后
  同修订证据核对，四段证据串同一修订。`docs/reports/p1-exit-evidence.md` 汇总出口证据，
  `p0-support-matrix.md` §7 追加 P1 行并统一保持 `Target`（归档评审前不升 Supported）。
  本 Change 已于 2026-11-20 归档，4 份 delta 已同步 `openspec/specs/`；支持矩阵 P1 行保持
  `Target`，升格 `Supported` 仍需独立评审确认。
  P2 工作接续 Change 已开工：切片 1.1 落地 `bod-app::handoff_package` 的 L1/L2/L3 等级骨架与
  纯准入判定（13 项命名用例）；未测组合的 L3 被拒并说明缺失证据，允许显式降级 L1/L2，
  自报/探测/过期证据不得冒充已测组合，准入不授予管理权。切片 1.2 落地
  `bod-app::canonical_json`（`bod-canonical-json-v1`）：键序/整数字面量/秒级 UTC 固定，
  `payload_digest` 只覆盖 payload 不递归自身，原始字节摘要并列且与规范摘要不同；
  14 条跨语言向量由 Rust 与独立 Python 校验器复算一致。切片 1.3 落地
  `bod-app::handoff_facts`：宿主事实与 Agent 叙述为两组不可互换输入，叙述无宿主字段写入路径；
  完成度只按宿主验证记录推导（自报/无证据不算），自称 done 无证据保持 `Unverified`，
  叙述改写 Effect 状态只记冲突、宿主状态不变（10 项命名用例）。切片 1.4 落地
  `bod-app::handoff_snapshot` 与迁移 `007_handoff.sql`：部分快照按 missing（带原因）/来源/
  逐片段完整性记录并可持久登记（同键同内容幂等、同键不同内容拒绝覆盖、关开后读回一致）；
  版本绑定只含 input/work_package/scene revision 与 `snapshot_seq`，三者变化才 `Drifted` 并使
  工作失效，序号前进为 `Superseded` 不失效，标题/标签/评论/时间变化只提示不失效（14 项命名用例）。
  切片 1.5 落地 `bod-app::handoff_manifest` 与 `bod-app::handoff_event`（迁移
  `008_handoff_events.sql`）：清单八类条目区分「在包内 / 只有指针 / 按授权引用 / 已脱敏 /
  未导出 / 清单级声明」，`content_digest()` 只从 `included` 报告，票据式指针的摘要不当内容证明；
  凭据只留名称与可复算摘要，含值文件与锁文件整条脱敏、自由文本替换为 `<redacted:NAME>`；
  六类事件按状态机折叠并重放校验，agent 只能 `requested/captured` 且限于自身 attempt，
  accept 幂等由 `(company_id, handoff_id)` 主键保证——重复请求返回同一序号与事件 id，
  不追加第二条事件、不新建第二份包或执行（14 + 17 项命名用例）。
  切片 2.1 落地 `bod-app::takeover_intent`（迁移 `009_takeover.sql`）：意图按
  `(company_id, takeover_key)` 幂等开单，阶段按 §19.1 八阶段推进并单独区分
  `awaiting_summary` 与 `aborted`；「等待总结」是持久状态而非事务，重启后重载仍为
  `awaiting_summary`、重复请求读回第一次时间而不推进版本，`invocations` 仍为 0；
  所有写入走 `WHERE version = expected` 条件提交，迟到写入报 `VersionConflict` 并保留
  先写者结果；进入 `attempt_created` 要求新 fencing token 严格大于旧值，但意图层只登记
  不创建 Attempt（14 项命名用例）。
  切片 2.2 落地 `bod-app::isolation_check`（无新表）：停止状态只由 P1 4.3 的取消事件派生，
  `ack` 永远不是停止确认（`evidence_ref()` 对未确认取值返回 `None`）；写入面按路径**组件**
  比较（`/ws/src` 与 `/ws/src2` 不相交），空枚举与未知面一律按「可能重叠」处理；隔离只接受
  环境销毁 / 写权限撤销 / 所有权移交三类机制证据且必须带引用，「这是独立工作区」不接受。
  写同一现场要求停止确认**且**已隔离，独立路径只要求已隔离——`Disjoint` 行里停止状态不参与
  判定，否则「允许独立无冲突工作」落不了地；相关外部副作用在停止未确认时一律拒绝重发。
  `clearance()` 作为 2.1 `isolation_checked`/`admitted` 的准入凭据，未通过报
  `IsolationBlocked` 且不推进版本（22 + 3 项命名用例）。
  切片 2.3 落地 `bod-app::effect_reconciliation`（无新表，案件复用 P0 `reconciliation_cases`）：
  hint 的不信任做成签名事实——`plan_queries` 没有 hint 参数、`EffectQuery` 无自由文本字段、
  `UntrustedHint` 只能 `screen()` 成「发现项 + 脱敏展示」；查询范围全由 `ConnectorRegistry`
  决定（连接器/操作/授权目标/参数名白名单，目标精确匹配，参数受长度与控制字符约束），
  缺路由记为 `unreconcilable` 而非 `skipped`；`Applied→Confirmed`、`NotApplied→Failed`、
  `StillUnknown→Unknown`，不明不能结案；`resend_guard` 只放行 `Prepared`/`Failed`；
  `block_scope` 取依赖闭包并同时给出 `free`，不冻结整个项目（18 项命名用例）。
  切片 2.4 落地 `bod-app::rebuild_precheck`（无新表）：摘要不匹配没有「继续」出口——`next_action()`
  在阻塞时只返回 `RemedyRequired`，选措施后只有「重取现场」或「人工处理」两种动作，`Rebuild` 仅在
  无阻塞且无待答问题时出现；比较只覆盖 `Included`，指针/授权引用/脱敏/未导出进 `unverified` 而非
  「通过」。`RequiredService`/`ToolchainPin` 按标识符核验（首版误当文件路径，实测报 `InvalidPath`
  与假缺失后修正）；忽略规则只作用于清单外文件，清单内文件命中同前缀仍照常比对。额外文件形成
  `UnexpectedFile` Question 且不自动删除；授权按 epoch/撤销/scope 当场复核，包内等级不是授权；
  预检命令只接受已登记引用，`NotRun` 不计通过；目标理解只有候选形态，歧义/冲突转 P1 Question 且
  责任方不由 Agent 填。`PrecheckClearance` 作为 `attempt_created → restored` 的准入凭据，缺凭据或
  attempt/修订不符即拒绝并保持原阶段（20 + 20 项命名用例）。
  切片 2.5 落地 `bod-app::continuation_card`（只读投影，无新表）：分项显示等级、保留成果、缺失、
  停止状态与重建步骤。「不把旧候选标为已接受」是类型事实——`PreservedArtifact` 字段私有、只能由
  宿主接受记录构造，与 `CandidateArtifact` 无互转；候选自称已接受时记不一致并保持未接受。等级只显示
  获批值（降级原因并列），权限原样回显（L1/L3 authority 相等、无授权位），`ack_only` 不渲染成
  「已停止」，隔离未检查为 `not_checked` 而非 `unknown`。实测问题：`approves()` 比的是 artifact
  revision，用它判工作包归属会让保留成果恒为空——已改为按绑定工作包字段判定并单列
  `stale_acceptances`（18 项命名用例）。
  切片 2.6 在 `docs/spec-notes/p2-takeover.md` §6 补阶段×场景×故障注入说明：十一阶段门禁逐条对应
  `check_transition` 分支，22 行注入矩阵只写测试里真做过的步骤，未覆盖项（真实进程停止、真实丢回包、
  旧经理并发切换、B 不可用、预算耗尽交接）标出归属切片而不假装覆盖。核对中发现两处不一致并记录：
  `captured` 阶段无快照引用门禁（补门禁会改 2.1 接口，留 5.1 前待定）；2.2 的「22 passed」是子串
  过滤把 `takeover_intent` 的一条用例计入，模块实为 21 条（已更正）。61 个被引用用例名逐一与
  `--list` 比对存在。
  切片 3.1 落地 `bod-app::manager_transfer` 与迁移 `010_management.sql`：委任按 scope 建部分唯一
  索引，库层直接保证「一个作用域最多一个 Active 主委任」；epoch 由宿主按头指针 +1 分配（`AppointRequest`
  没有 epoch 字段），撤任同时推进 epoch 让旧票当场失效，重新启用同一 Agent 也是新 epoch。复核入口
  每次读权威头行，缓存条目只在头版本未变时可用——暖缓存挡不住撤任后的旧 epoch（实测断言命中数与
  重读数）。管理票证绑定 appointment/epoch/scope/命令集/受众/时效，超过 900s 或过期/未生效/有效期
  超出委任到期分别拒绝；票证没有 invocation/assignment 字段，不是执行凭据；治理命令在构造命令集时
  就被拒。拒绝一律落 `management_commands` 审计（原因码+说明），且拒绝前后头行与委任行逐字段相等
  ——本模块没有改预算/计划/派发/成果的代码路径。实测修正两处：撤任后「头 epoch 大于委任行 epoch」
  是正常态，首版当成 `CorruptScopeHead`；epoch 因撤任也占一格，历史是 1/3/5 而非 1/3/4。暂停不递增
  epoch，且暂停期间不能直接换人（原子切换属 3.3）（23 项命名用例）。
  切片 3.2 落地 `bod-app::staff_continuation`（只读投影，不建表）：执行座位只能从 Issue 的
  Assignment 读出（构造入口没有 Appointment 参数），管理身份只来自当前委任，双向顶替都没有代码
  路径；`Directive` 没有 Cancel/Stop/Restart/Redo，所以「切换不停止员工、不重跑、不重做」是类型
  事实而非开关。已接受成果按「未取代 + 非待人工 + 接受指针指向该修订」保留，其余进
  `stale_acceptances` 单列可数；旧经理兼任执行按 §19.10 分支——只有宿主显式声明可分离才 `Continue`，
  未声明/不可分离/座位未登记一律 `RebuildViaHandoff` 指向 2.2/2.4/2.5，本模块不自己停进程。实测
  A→B 切换后运行/接受/贡献逐字段相等、计划逐条不变、旧票 `StaleAppointment`，员工项与「谁走」
  无关。同时补齐 `xtask migration-check` 自 007 起未登记的四个 P2 迁移（该检查此前一直是红的）
  （19 项命名用例）。
  P2 的 I/G 绑定仍全部 planned，
  `gates.yaml` 与 `BINDING_SOURCES` 留到 P2 出口按权威语义统一补齐。
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
- 里程碑出口：相关 I/G 门禁有真实证据后才勾选 tasks.md；每个阶段 Change 在自己出口证据齐全后归档，P0 已于 2026-11-19 归档，后续阶段不回溯修改 P0 历史约定。
- 工具链调用前先 `export PATH="$CARGO_HOME/bin:$PATH"`（详见 `docs/compat/toolchain.md`）。
