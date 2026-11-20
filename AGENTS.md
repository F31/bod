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
  切片 3.3 落地 `bod-app::takeover_review` 与迁移 `011_handover_review.sql`：检查点只存
  `(kind, id, fingerprint)` 七类事实指纹，游标是对排序文本求 sha256（只覆盖事实、不覆盖缺口），
  读回重算并核对条数，超限报错不截断；增量分新增/变化/消失三类，缺口（无旧经理总结、部分快照、
  未知执行、待人工验收）单独可查。接管确认在 `BEGIN IMMEDIATE` 事务里读头+委任+检查点再插行，
  绑定 epoch 与游标，`confirmation_as_ticket` 恒错——「只读确认不产生管理权」是类型事实；
  确认状态随 epoch 走（撤任即 `Superseded`），确认后命令仍逐条复核旧票 `StaleAppointment`。
  该切片新增两张表（迁移 11 条 / 表 46 张），`migration-check` 同步登记（14 项命名用例）。
  切片 3.4 落地 `bod-app::history_continuity` 与迁移 `012_history_disputes.sql`：历史连续分三段
  ——数据库 trigger 阻止改写（012 补上 005 漏掉的接受五元组/理由/证据冻结，并给取代指针加单调
  约束）、只读视图发现改写（六类事实指纹 + 游标，内容变化/指针回退/整行消失与正常追加分开判）、
  争议只标待核查（`history_disputes` 一行 `under_review`，结案不改被争议的记录，推翻只返回指向
  P3 的提案要求）。「重验旧成果」只有「保留旧接受 + 要求新提案」一个出口，改写接受、改费用主体、
  删历史三个请求在类型上恒错。核对中发现 `decisions` 表没有不可变约束，本切片只发现不阻止并如实
  记录（迁移 12 条 / 表 47 张，21 项命名用例）。
  切片 3.5 落地 `bod-app::scope_boundary` 与迁移 `013_scope_org.sql`：专项边界靠新增层级表
  `issue_hierarchy`（不改 `issues`/域对象，一次写定，载入查悬空与环）解析「最近的专项祖先优先」，
  项目负责人越界指挥报 `CrossesSpecialBoundary`、在子树内建任务报 `BypassAttempt`、专项越出子树报
  `OutsideScope`，撤任后子树回到项目委任；嵌套专项只由治理主体创建且只返回作用域。reportsTo 只写
  `agent_org_metadata`，写入前后比对 issues/runs/leases/grants/appointments 摘要，「没有迁移状态」
  是比对结果而不是声明（迁移 13 条 / 表 49 张，16 项命名用例）。
  切片 3.6 在 `docs/spec-notes/p2-manager-transfer.md` §6 写出负责人切换操作手册：§41.8 出口的
  M1—M12 每行给出接口、可执行 `cargo test` 与稳定拒绝码，§6.2 列出四个反例与阻断码，§6.3 给十步
  顺序，§6.4 明确 N2/N4 没有样本并归属 5.1/4.4；新增 `tests/p2_manager_transfer_manual.rs`
  逐条核对 ID、命令与错误码，文档与代码脱节即失败（3 项自检）。
  切片 4.1 落地 `bod-app::conformance_report` 与 `bod conformance` 子命令：报告按适配器版本/协议/
  平台/能力/日期分项，能力判定七态（`verified` 只在「声明 supported 且映射用例全 pass」时出现），
  且**没有**单一通过标记——`overall_badge()` 恒返回 `NotReducible`、markdown 无 `PASS`、JSON 无
  `passed`。能力→用例映射只保留 `cancellation → cancel-request-vs-confirm`，拒绝把宿主持久去重、
  ACK 推进、迟到费用接收当适配器能力证据，因此 Fake 的 `external_idempotency` 如实判为
  `declared_only` 而非 `verified`（11 项命名用例 + 3 项 CLI 用例，详见
  `docs/spec-notes/p2-conformance.md`）。
  切片 4.2 补齐九类必需用例（用例集 10 → 14，新增 `scope-identity` / `protocol-version` /
  `sensitive-redaction` / `late-observation`），把用例分为 identity/authorization/integrity/
  behaviour 四类，`enablement(report)` 三态 `Enabled`/`Unverified`/`Blocked`——闸门类失败即禁用、
  失败优先于未测，且「degraded 不能豁免」是结构保证（入参只有报告，读不到描述符）。同时新增
  `observation_redaction` 并在 `receive_observation` 落库前生效。**修复真实缺陷**：`invocations`
  主键全局唯一但 `submit_start` 未校验公司，跨公司同 ID 会返回别家 `execution_id`（现报
  `E_SCOPE_MISMATCH`）；审计 outcome 枚举缺 scope 取值，复用 `params_rejected` 并如实记录分类不精确。
  另把 `local_process` 真实进程用例的观察窗口从 2s 提到 10s（断言未放宽），因新增测试后全量并行
  时解释器启动超过 2s（详见 `p2-conformance.md` §2）。
  切片 4.3 落地 `bod-app::capability_dispatch`：交接等级换算成保证需求（L1 无 / L2 可靠观察 /
  L3 会话恢复+可靠观察），`select()` 按五种稳定原因过滤候选（`adapter_blocked` 优先，
  `capability_stale` 不算支持），`admit_handoff` 把能力门与已测组合绑成唯一承接入口（已测组合
  不能弥补能力缺失）。「界面与调度同一限制」靠单一文案来源 `CapabilityNotice` 保证，
  CLI `bod adapter capabilities --need l3` 与调度共用同一渲染函数；实跑本机适配器得到三条
  真实原因。如实记录：派发路径仍是单适配器写队列，`admit_handoff` 尚未被 `request_dispatch`
  调用，故不宣称「已在调度器禁用」；准入按声明+有效期而非 4.1 的 verified，风险由 4.4 承担。
  切片 4.5 落地 `bod-app::release_acceptance`：六项发布验收（停止/重放/副作用/账号预算/证据/日志）
  各三态 `pass`/`fail`/`not_evidenced`，只有六项全 pass 且三处无分歧才 `releasable`，
  `can_mark_supported()` 与结论同源；日志扫描复用 `observation_redaction`，**证据只存脱敏后的行**
  （两个出口都断言不含原文凭据）；`align()` 按集合比对探测/准入/界面三处的缺失能力，四种分歧任一
  即 `blocked`。CLI 新增 `bod adapter release-check [--logs] [--need] [--json]`。实跑本机适配器
  `--need l3` 得 `blocked`（`replay_reconnect=no_dedup_retention` 等），三处一致为
  [recovery, reliable_observation]。如实记录：CLI 不打开业务库，故「重启意图仍在」与
  「账号预算可评估」在本机恒为 `not_evidenced`，不补默认值；`acceptance_evidence` 的 pass
  只表示有证据且未过期，本机 `enablement` 仍是 `unverified`。详见 `p2-conformance.md` §4。
  切片 5.1 落地 P2 故障验收 `crates/bod-app/tests/p2_fault_acceptance.rs`：六个命名 case 从外部
  按场景驱动真实代码（停旧写入 / 脏文件 / 丢回包 / 崩溃中交接 / 旧经理并发切换 / B 不可用），
  前两个用真 git（仓库 + 两个 worktree + 真实 diff），验证的是**跨模块接缝**而不是各模块自证。
  跑出两个细节：撤任后旧票证命中 `stale_appointment`（`revoke` 递增 epoch，比 `appointment_inactive`
  更强）；`KeepDirty` 的 `dirty_paths` 与 `changed_paths` 一致。**这不是门禁结论**：
  `gates.yaml` 未改，G-28—G-30/G-39—G-41 仍 planned，转 implemented 由 5.3 裁定；
  「B 不可用」用 `Enablement::Blocked` 表达而非真下线。详见 `p2-fault-acceptance.md`。
  切片 5.3 落地出口汇总 `docs/reports/p2-exit-evidence.md`（结论 `blocked`，阻塞项 4.4/5.2）：
  汇总 4.1—4.3、4.5、5.1 的机内证据与两类「必须由人跑一遍」的缺口；支持矩阵追加 §8
  （接续/双运行时/对照指标 `Planned`，三项机内已实测记 `Unverified`，无一行 `Supported`）。
  `tests/p2_exit_evidence.rs` 做漂移检查（6 条），并反向验证过：把结论改成 `ready` 立即失败。
  **5.3 未勾选**——验收要求的「与手工交接对比」依赖 5.2 的人工数据，4.4 缺第二运行配置与人工耗时。
  为解 4.4 的能力缺口新增 `bod-app::durable_process`（`adapter.durable-process v1.0.0`）：
  stdout/stderr 追加进会话目录事件文件、序号=行号、**不持久化读游标**（重启从 0 重放，由宿主
  inbox 按序号去重），因此能以 test_result 声明 `recovery` + `reliable_observation`；宿主不在时
  子进程已死则报 `Unknown` 而非终态。`Drop` 不杀子进程（否则无法重启接管），孤儿由 cancel 收尾。
  11 条真实子进程用例；完整 conformance 实跑 pass=5/fail=0/na=9。**跑出结构性冲突**：闸门类
  `auth-expired`/`sensitive-redaction` 需故障注入，真实适配器恒 `not_applicable` →
  `enablement=Unverified` → `select()` 永远过滤真实候选（今天没有任何真实执行端可被调度准入）；
  两条可选解留待评审，不单方面放宽证据。详见 `docs/spec-notes/p2-durable-adapter.md`。
  **该冲突已按「不改判定阈值、改用例驱动方式」解掉**：`auth-expired` 改为按声明的凭据面分支
  （`unsupported` 时验「诚实界定 + 授权约束仍生效」，含跨公司 `E_SCOPE_MISMATCH`）；
  `sensitive-redaction` 改为真实负载驱动，含凭据负载由 conformance 入口通过
  `use_conformance_payload()` 配置，不由适配器自选；`dispatch_case` 返回
  `CaseFailure::{Fail, NotApplicable}`，用例可依运行时观察自判不适用。结果：两个真实配置都是
  `pass=7 fail=0 na=7`、`enabled`；`capabilities --need l1` 从「无候选」变为
  `BOD_CAPABILITY_SATISFIED`，`--need l2` 只剩 `capability_not_supported`（诚实的能力边界）；
  `release-check` 的 `effect_authorization` 由 `not_evidenced` 变 `pass`（整体仍 `blocked`）。
  已知漏检：`observation_redaction` 不认 `--token value` 空格分隔形态，未顺手改。
  详见 `p2-conformance.md` §5。
  补上 **4.4 的采样驱动**：`bod-app::handoff_sample` + `bod handoff sample`。样本
  `sample.resume-from-snapshot/v1` = 产出两个检查点 → 写交接快照（含真实 `last_sequence`）→
  确认停止 → **新实例**读快照续跑，首行必须回声 `resume from <序号>`（防「从头重跑也算接续」）；
  L2 再加「新实例重放旧观察、序号连续、重放判 duplicate」。实测 L2 = `evidenced (2/2)`
  （Python 3.13.14 与 node v22.22.2 各一个 `durable-process` 配置），`local-process` 因声明
  `reliable_observation: unsupported` 记 `unsupported` 不计入；L3 报 `unsupported`（样本无核验步）。
  **4.4 仍未勾选**：负责人切换只有库内 case、人工分钟只有脚本化的 0——两项都要人真实跑一遍。
  实测见 `docs/reports/p2-handoff-samples.md`，设计见 `p2-handoff-sample.md`。
  把「人工耗时」从可随意填的数字改成可追溯读数：`bod handoff drill start/mark/finish/show/arm`。
  时间戳由系统时钟写入 `drill.timeline`，耗时 = `handoff_done` − `last_action`（算出而非输入）；
  四条拒绝规则（每档一次、时间戳不回退、封存后拒绝追加、缺档位拒给读数）有 11 条命名用例；
  `HumanTime` 拆 `Scripted`/`Drilled`/`Claimed`/`Unrecorded`，口述值进报告但排除出
  `人工耗时证据: n/4`。5.2 的四项指标走 `--arm manual|bod` 收口，报告含样本数与未完成项、
  不写提升比例。通路已冒烟（记为「非真实演练」），**真实读数未取得**，故 4.4/5.2 仍不勾选。
  协议见 `p2-handoff-samples.md` §6，设计见 `p2-handoff-drill.md`。
  P2 的 I/G 绑定仍全部 planned，
  `gates.yaml` 与 `BINDING_SOURCES` 留到 P2 出口按权威语义统一补齐。
- **P3 进行中（2026-11-21）**：`openspec/changes/p3-change-collaboration` 24/26 任务已勾选。
  四个能力各有模块与 spec note：`openspec_integration`（规格修订/任务稳定身份/拆分检查/进度投影/
  归档串行）、`impact_revalidation`（显式与推测边分离/影响评估/有界重验/维护提案）、
  `merge_queue`（三道门：队首/基线/证据，契约冲突先到先得，worktree 只能算协作隔离）、
  `collaboration_plan`（答疑零副作用/顾问命令被拒/执行图准入/在途 Run 保护/会话键六分量）。
  **2.6 与 5.2 未勾选**：G-31 与「共享接口变化从发现到再次接受」需要真实项目样本，
  机器造不出来。出口结论 `blocked`（`docs/reports/p3-exit-evidence.md`），支持矩阵 §9 无 Supported 行，
  P3 的 I/G 绑定同样全部保持 planned。CI 见 `.github/workflows/ci.yml`（Linux + Windows + workbench + openspec）。
- **P4 进行中（2026-11-21）**：`openspec/changes/p4-runtime-delivery` 任务 1.1—1.6、2.1—2.6 已勾选。
  `bod-app::runtime_verification` 把运行验收拆成五件事：计划必须覆盖五个面且 API/浏览器/Agent
  三类检查必须指名已有工具（自建引擎被拒）；报告绑定 commit/digest、规格与规则修订、环境、
  工具版本、执行者、时间与原始引用，不匹配分 `StaleCommit`（`invariant_i_08`）、`DigestMismatch`、
  `NeedsRevalidation`、`Unknown` 四类且只有 `Usable` 可接受；`BuildRecord` 一次构建按 digest
  晋升，平台进身份摘要，平台变化强制重建重验；门禁为 Required/Advisory/Manual ×
  Pass/Fail/Unknown/NotApplicable，未运行不是 Pass、`NotApplicable` 需豁免、豁免单独留痕；
  失败分类只有业务断言失败归咎代码，只换执行人不算独立验证。17 条命名用例 + 6 条漂移检查，
  说明见 `docs/spec-notes/p4-verification.md`。
  `bod-app::stage_manifest` 承接对外表达：Manifest 冻结修订（浮动引用与名称一律拒绝）+
  `base_revision` 乐观并发（过期即 Conflict，候选保留不动指针）+ 外部变化只登记 `PendingReview`；
  `Publication` 的签发者/连接器声明未宿主核对即 `Unconfirmed`，`Fixed` 只在「已确认投递 + 验收通过」
  出现；internal/customer/audit 三视图按字段类别裁剪，凭据在**任何**视图都不出现，客户视图只报
  被扣数量；内容清单身份（排序后摘要）与运输文件身份分离，签名必须写明覆盖 payload，脱敏包不得
  声称原始哈希链连续；离线校验固定五项分列，自签/嵌入公钥只证明包内一致（`invariant_i_52`、G-33）。
  16 条命名用例 + 5 条漂移检查，说明见 `docs/spec-notes/p4-evidence.md`。
  `bod-app::release_governance` 与 `bod-app::maintenance` 承接发布治理：`EnvironmentRef` 分开记
  desired/observed，只有相等才投影为已部署，观测缺失即 `Unknown`，Manifest 明文拒绝密钥字段；
  `Approval` 绑 digest/环境/参数/有效期，改目标即失效（AZ05）；部署状态机要求提交后派发，
  回包丢失进 `Unknown` 并禁止盲重发，查询须带证据落定；回滚只恢复已声明可回退资源，
  不可逆迁移与前向修复分列；预算解释按「限额 − C − R − U」算可用额度，不足即拒且只读；
  `maintenance` 按去重键生成只读提案，预算到限停止、白名单外检查拒绝、恒不升级不发布（G-35）。
  20 条命名用例 + 5 条漂移检查，说明见 `docs/spec-notes/p4-release.md`。
  **4.x—5.x 未开始**：覆盖回执与出口证据需要真实环境与真实业务流程，未接入前不标
  Supported；P4 的 I/G 绑定全部保持 planned。
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
