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
  指令优先级与 Question 归口、会话复用与角色/Profile/授权重置（I-36、G-11 implemented）；
  片段缓存已按 `cache.max_entries` / `cache.max_bytes` 有界（LRU、非预分配）并支持被淘汰片段回源，
  G-48 由 `unverified` 升为 implemented（撤权时拒绝命中且不得借回源绕权）。
  `bod-app::desktop_lifecycle` 提供连接归属、退出顺序、单宿主租约、sidecar 平台清单与支持等级判定，
  但 G-51 仍为 `planned`：真机 Tauri/WebView 与平台矩阵 conformance 报告未取得，单测不冒充门禁通过。
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
- **P3 任务完成（2026-11-21）**：`openspec/changes/p3-change-collaboration` 26/26 任务已勾选。
  四个能力各有模块与 spec note：`openspec_integration`（规格修订/任务稳定身份/拆分检查/进度投影/
  归档串行）、`impact_revalidation`（显式与推测边分离/影响评估/有界重验/维护提案）、
  `merge_queue`（三道门：队首/基线/证据，契约冲突先到先得，worktree 只能算协作隔离）、
  `collaboration_plan`（答疑零副作用/顾问命令被拒/执行图准入/在途 Run 保护/会话键六分量）。
  **2.6 已勾选**：`xtask impact --from/--to` 从本仓库真实提交（`a9c80d5`、`157298b`）与真实
  模块图跑 G-31，接口变化 3+1 处、确定 61+9 个、候选占已知区域 5.3%，G-31 已升 implemented。
  报告只给确定/可能/未知与未执行项；**静态图会漏** `use super::`、`use crate::{a,b}` 多导入、
  `pub use` 重导出，宏与动态派发不可见（`docs/spec-notes/p3-impact-sample.md` §6），
  故不宣称"只有这些受影响"。**5.2 已勾选**：`xtask revalidate --from/--to` 把同一对真实提交走完
  发现 → 提案 → 真跑重验 → 漏项核对 → 再次接受，Linux/Windows 各两条记录
  （`docs/reports/p3-revalidation.md`）。三条读数必须记住：漏项恒为 2
  （`bod-desktop`/`xtask`，rustc 包级重编传播，静态模块图不追踪）；无效重验靠
  "退出码 0 且零用例"判定，第一版用 `--lib`/`--bins` 当全量时真被它抓到过
  （`bod` 的 `sse_stream_e2e` 从未被覆盖）；协调与集成失败为 0，因为两提交顺序、
  改动文件集无交集，**没有编造冲突填这一栏**。Windows 连续跑会撞 `LNK1104`
  （测试子进程持有产物），必须先 `cargo clean -p bod -p bod-app`；
  这类失败记为 `build-failed`（工具环境），与用例 `failed` 分开统计、不改判。
  出口结论「任务完成，支持等级未升级」（`docs/reports/p3-exit-evidence.md`）：`G-7`
  （多 Agent 并行合并）保持 `planned`，支持矩阵 §9 仍无 Supported 行。
  CI 见 `.github/workflows/ci.yml`（Linux + Windows + workbench + openspec）。
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
  `bod-app::coverage_receipt` 承接逐客户覆盖与回执：`Fixed` 需「制品已提供 + 部署已观察 +
  验收通过 + 客户确认」四件齐，只有制品时答复固定为「修复版本已提供，客户环境待核实」；
  覆盖按项目/环境/部署单元/制品摘要四元组判定，人群未知、开关未同步、验证未过、缺制品关系
  一律待核实，离线安装只认授权实施人回执并标人工来源；回执投影不含客户 id 与内部备注，
  打开链接只记访问，列表计数不暴露他人；回滚重开义务并只更正已答复客户、历史版本不变，
  投递键去重且渠道不可查证时标待核实；证明页四类分列，缺 AcceptanceReceipt 即待核实；
  结算以（里程碑, 回执）幂等，争议/撤回进复核但用量仍按实际计。
  15 条命名用例 + 5 条漂移检查，说明见 `docs/spec-notes/p4-coverage.md`。
  **出口未达（2026-11-21）**：5.1 已跑（G-9/G-24/G-32/G-33 各有命名用例与真实断言，
  `tests/p4_gates.rs` 守"不得提前写进 gates.yaml"），5.4 已出报告
  （`docs/reports/p4-exit-evidence.md`，结论 `blocked`，阻塞项 5.2、5.3）；支持矩阵 §10
  为 4 Unverified + 2 Planned + **0 Supported**。**5.2 与 5.3 未勾选**：一条真实业务流程的
  最终制品/环境组合、三类产品对照基线都需要真实环境与真实样本，机器不能生成。
- **P5 进行中（2026-11-21）**：`openspec/changes/p5-team-reuse` 任务 1.1—1.6、2.1—2.7、3.1—3.7、
  4.1—4.5 已勾选；5.1 与 5.3 已勾选，**5.2 未勾选**。
  `bod-app::catalog_upgrade` 承接协作配置包的四段边界：安装前五道校验（确定版本 → digest →
  来源 → 许可证 → schema，再判管理员权与内嵌密钥，本地文件默认关闭）；`InstallTxn` 五阶段，
  联网下载不占写事务、候选在 `Applied` 前不执行、没预览不许批准、硬冲突挡在批准前；
  `CatalogInstallRecord` 逻辑键为「Company|包|版本|digest|参数摘要」**不含展示名**，重复请求返回
  原记录且不推进修订；同名对象改 id 落新的一份，绝不按名称覆盖；三方升级按 digest 判十一种
  情形，两边都改与「本地删了上游又改」都落 `Pending` 提案而非静默覆盖（`invariant_i_44`），
  新增权限/外部程序/付费连接逐条重新准入，批准不产生 Grant；卸载恒不级联删除，Issue/交付/
  用量/证据四类保留；能力目录与 Team Catalog 数据身份分离，推荐与 `reports_to` 结构上不参与
  授权判定。14 条命名用例 + 5 条漂移检查，说明见 `docs/spec-notes/p5-catalog.md`。
  `bod-app::authorization` 的 P5 段承接六阶段权限：十个动作映射六种要求且**跨段不派生**
  （范围批准权换不到分派与预算、接受权换不到发布、一人兼任逐项写依据），拒绝必带
  actor/动作/范围/依据；过滤先判可见再判可用，客户协作者只见自己客户、`total` 只等于可见条数；
  执行绑定把 invocation/项目/租约钉住，跨项目兑换被拒且审计只有引用 id 没有值（AZ02）；
  验收四条拒绝互不替代（旧修订 → 测试人角色 → 执行者自关闭 → 接受权），测试报告不能替代接受；
  待发动作前核六项，被拦时连接器调用数为零（AZ05）；撤权停用全部 Grant、保留 Assignment 与资产、
  在途发停止请求、无法撤销的外部凭据进对账。15 条命名用例 + 4 条漂移检查，
  说明见 `docs/spec-notes/p5-permissions.md`。
  `bod-app::feedback_knowledge` 承接反馈到规则的晋升：分类先判治理（发布生产/权限/密钥/预算/
  已批准验收条件五类转独立流程，不写 Profile），「以后」「所有项目」收窄到项目而不当永久全组织；
  提案带 `base_revision` 乐观并发，冲突保留候选且当前修订不动，旧修订摘要合并后逐字节不变
  （`invariant_i_14`、`invariant_i_19`）；自动接受要同时满足已认证来源、允许字段、作用域上限、
  有效期与审计，外部网页/PR 评论/工具输出/Agent 自述一律交人；发布产生新修订与新指针，
  下一次 Attempt 钉住后不再切换，回滚是"发布旧内容的新修订"且只刷新会话、不撤账本
  （`invariant_i_23`）；缓存只按 revision+digest 精确命中并按公司/项目/权限过滤，删除与撤权即
  miss，Manifest 与 CAS 永不淘汰（G-48 回归）；`EvaluationRecord` 样本不足即拒、不同任务集不可比
  （G-13）。15 条命名用例 + 5 条漂移检查，说明见 `docs/spec-notes/p5-knowledge.md`。
  `bod-app::notification_channel` 承接通道：三类卡片从同一事实投影且必须有受鉴权入口，
  正文只有摘要（代码/日志/附件只进 `withheld`，纯文本通道同样不内联）；命令绑成员/项目/目标/
  修订/命令键，离线只显示待同步、撤权/超期/旧修订必须刷新卡片、只有权威回执才显示成功；
  渠道 `Accepted` 只算"收到请求"，`Delivered`/`Read` 要渠道观察，更新范围由收件人/环境/线程/
  外部消息标识四项限定；更正按投递键（回执/目标/渠道/收件人/修订）去重，可编辑渠道改原消息、
  只可追加渠道带原回执编号，回包丢失先查证、无幂等能力标待核实；入站只看认证身份与高熵限域
  令牌，通道文本从不参与判定。12 条命名用例 + 5 条漂移检查，说明见 `docs/spec-notes/p5-channel.md`。
  **出口未达（2026-11-21）**：5.1 已跑（G-13/G-21/G-22/G-25 各有命名用例与真实断言，
  `tests/p5_gates.rs` 守"不得提前写进 gates.yaml"且不改写 G-21 的 P0 绑定），5.3 已出报告
  （`docs/reports/p5-exit-evidence.md`，结论 `blocked`，阻塞项 5.2）；支持矩阵 §11
  为 4 Unverified + 2 Planned + **0 Supported**。**5.2 未勾选**：AZ01—AZ06 六阶段授权与隔离
  验收需要真实多主体（成员 + Agent + 客户 + 连接器）与真实网关，机器不能生成；
  P5 的 I/G 绑定（G-13/G-21/G-22/G-25、I-14/I-19/I-23/I-44）全部保持 planned。
  P4 的 I/G 绑定**全部保持 planned**。
- **P6 进行中（2026-11-21）**：`openspec/changes/p6-open-extension` 24 项任务中 22 项已勾选；
  **2.8 与 4.6 未勾选**（需真实双机与真实 iOS/Android 设备）。出口结论 `blocked`，
  证据见 `docs/reports/p6-exit-evidence.md`，支持矩阵 §12 为 0 Supported / 3 Unverified /
  5 Planned / 2 Experimental；G-19/G-23/G-34/G-46/G-47 不得写入 `gates.yaml`，
  由 `tests/p6_gates.rs` 与 `tests/p6_exit.rs` 守。
  `bod-app::extension_sdk` 承接第三方适配的接入边界：描述符带版本/平台/schema 版本/能力分项与
  限制，握手先判协议版本再逐项判必需能力与证据门槛，**拒绝只作用于新扩展任务**（缺 `Delegation`
  的旧适配器仍走原本地路径）；安装按「版本 → digest → 来源 → 许可证 → 协议 → schema → fixture →
  内嵌密钥 → conformance」顺序判，没有该 schema 版本的 fixture 一律拒装，"能解析"不等于"我们测过"；
  无 conformance 报告只可 `Experimental`，不进调度候选、不标 Supported，报告张冠李戴按
  `ReportAdapterMismatch` 拒装；四级证据 `SelfReported < Probed < ContractTested < RealTask`
  不等价，三项安全项（身份/授权/消息完整性）任一失败即 `Unsupported` 并禁用派发，降级徽章不豁免
  （`invariant_i_57`），界面徽章与 `dispatch_enabled` 同出 `view_for`；工具清单摘要排序去重后比对，
  可执行摘要/清单/`schema` 任一变化即 `Required`，旧批准不沿用；出站材料按项目/范围/接收方/账号
  四项比对，无决定即 `ZeroDelivery`，扩大范围必走 `NeedsDecision`，审计只留引用与摘要（SB06）；
  `support_claim` 让"支持声明"成为机器可检结构——确定版本 + 具体平台 + 对应报告，自报级只能说
  自己是自报级。11 条命名用例 + 6 条漂移检查，说明见 `docs/spec-notes/p6-sdk.md`。
  `bod-app::remote_delegation` 承接远端链路：节点与 webhook 共用一份启动结构，参数摘要不含重试与
  时间，同 ID 同摘要重放回原实例、异摘要拒并保留原记录（RM01），管理动作的 Appointment 与
  authority_epoch 各自独立成拒绝理由；批次带密钥版本与起止 sequence，验签 + 去重 + 连续推进一次做完，
  缺口只要求补传不跳序，`requires_acceptance_evidence()` 恒真、`accepted_artifact()` 恒假（RM07）；
  设备有效动作是「设备能力 ∩ 用户项目权限 ∩ 已启用通道」，配对与在线都不放宽，新通道默认关闭，
  关闭即终止控制会话、拒绝排队命令、在途外部操作转对账（SB05）；慢订阅超限整段释放缓冲、按最后
  持久游标补拉、游标过期读快照基准，没有权威回执一律显示待核实（RM06）；`bod-app::event_stream`
  + 宿主 `GET /_bod/events` 已提供真实 SSE 基线传输（Bearer 鉴权、`Last-Event-ID` 优先、
  快照基准与 `resync`+`retry`、心跳保活、查询串令牌直接拒绝），真实 TCP 用例覆盖 7 种情形；
  信箱满额与过期明确反馈，
  同一 decision_key 至多一次有效业务决定，冲突方记名而首个保持权威（RM09）。
  `bod-app::remote_partition`（G-19）承接分区与对账：失联暂停新外部动作、连接显示"未知"、未上传
  事件不丢弃、查看继续；恢复前核对 epoch/租约/尝试/游标/外部动作五项，一项不符即列出全部原因；
  过期 token 不得改宿主状态，旧尝试迟到只进历史、不产生接受、预算按实际用量对账（RM03）；
  外部动作按稳定业务键查询，已生效只补记一次、确认不存在才一次补写、不可查询显示待核实且
  零盲写，目标不能隔离旧写入即阻塞并点名责任人（RM02）。16 条命名用例 + 6 条漂移检查，
  说明见 `docs/spec-notes/p6-remote.md`。
  `bod-app::readonly_api`（G-34 / I-55）承接只读适配：七类只读动作的副作用计数结构性为 0
  （不建 Run/Binding、不发 invocation、不启动 Agent），有界上限 200 行且截断如实标记；
  按钮提示只产 available/hidden 两个字符串，放行只由 `authorize_readonly` 判定；
  Company→Project→客户→工单四层过滤在返回内容前完成，`total` 只等于可见条数、`withheld`
  只有数量，导出文件名只含可见行数，附件不可见与不存在给同一答案；订阅每次续传先鉴权再补拉，
  撤权即拒、游标超保留期读快照基准；投影缓存键含身份/输入/索引/工具四分量，
  Manifest/账本/租约/幂等回执/ACK 游标永不进可淘汰集合，授权修订或 TTL 变化即 miss 并显示重算中；
  审计只留 `principal:`/`object:`/`project:` 引用与策略修订，无令牌与密文。8 条命名用例 +
  6 条漂移检查，说明见 `docs/spec-notes/p6-readonly.md`。
  `bod-app::mobile_control` 承接移动与 H5 分期：阶段三级有序，宣称"手机完成全流程"要 iOS/Android
  真机 + MD01—MD04 六项齐备，缺项逐条列出，模拟器不算真机；设备签名按「撤销 → 生物识别 →
  成果摘要 → 修订」顺序验，旧签名不接受新成果，重签才出回执；离线命令有容量与投递窗口，
  授权到期只换令牌不生效、窗口过只返回待投递，提醒推送零业务副作用并升级给有权主体；
  H5 门户三项必须同时为「没有」（无工具启动权、无工作区挂载、无生产凭据），打开链接只记访问；
  重复确认只记一次决定，回滚推进序号并留痕、证明页显示当前覆盖与历史，成本与结算分别核对，
  撤权后离线重放一律不生效。10 条命名用例 + 6 条漂移检查，说明见 `docs/spec-notes/p6-mobile.md`。
  **4.6 真机验收未做**：没有任何一台真实 iOS/Android 设备参与验证，"手机完成全流程"在任何口径下
  都拿不到 `Allowed`。
  **P6 的 I/G 绑定（G-19/G-23/G-34/G-46/G-47、`invariant_i_15`/`invariant_i_57`）保持 planned**：
  `Supported` 只表示该适配器自己的闸门用例全绿，Fake Executor 即使跑通自己的套件也照样
  `is_production_capable() == false`，自测报告不是生产能力，支持矩阵不因此升 Supported。
- **P7 容量评估（进行中）**：口径与治理在 `bod-app::capacity_evaluation`，实测在
  `bod-app::perf_probe`（入口 `cargo run -p xtask -- bench`，可 `--out` 落盘；`cargo run -p bod -- perf`
  同一份测量）。三条不许松的口径：①状态三档 `Target`/`Measured`/`Unverified`，缺样本或缺命令
  的报告不进入容量决策，`Target` 项让整份报告退出决策；②形态分报，headless 的空闲 RSS 不得
  冒充桌面总内存，`may_claim_form_factor` 按形态逐项成立；③吞吐与尾延迟分别是两个口径
  （`event_throughput_batched` 每批一次 fsync，`event_throughput_durable` 每条一次），
  任一项未达标 `may_claim_all_targets` 即为 false 且 `unresolved()` 点名。毕业判定只看四类信号
  （RTO、单写争用、体积压力、多机/容灾），日均事件数只是观察值，`graduation(&[], Some(n))` 直接
  返回 `RejectedByEventCount`；方案对比必须四类齐备且四项记录不空，DAO 替换一律不算语义兼容；
  `GraduationDecision` 只追加（`record` 拒覆盖、`revise` 追加并保留历史），缺项进不了日志；
  容量阶段内任何换引擎/降 `synchronous`/放宽事务/跳门禁都被 `guard_change_in_capacity_stage` 阻断，
  性能数字也不得扩大支持矩阵（`guard_support_promotion`）。16 条命名用例 + 8 条漂移检查，
  手册见 `docs/spec-notes/p7-measurement.md`，台账见 `docs/reports/p7-baseline-<platform>.md`。
  **未取得的部分**：`cold_start_to_api_ready`（bench 不启动宿主监听）、Windows 侧 `idle_rss`
  （读不到 `/proc/self/status`）、`idle_cpu_ratio`、Web 可见延迟、桌面三项与桌面包体积、移动包体积
  全部保持 `Unverified` 并写明缺口；`May claim` 因此为 false，报告不会给出"全部达标"。
  §2 归因判定在 `bod-app::bottleneck`，三条不许含混的口径：①恢复 RTO 必须拆成外部复制与内部
  重开/校验，缺任一侧 `classify_recovery` 直接 `Unverified`；②单写争用要同时看 Busy 与 p99 增长，
  `busy_timeout` 取默认值时连接是**等待**而非报错，所以 Busy 通常为 0，`QueueingWithoutBusy`
  只记排队与 unknown、不判成 Busy 争用；③体积压力（库大小/`REINDEX` 成本）与索引空间
  （未索引全表扫 vs 索引等值的倍数）分开评，`events` 的 `aggregate_id` 已有 UNIQUE 自动索引，
  不能当"未索引"对照。`classify_slots` 用单写与最宽并发的吞吐比判 `WritePathBound`/`ScaleExecutors`，
  `agent_slot_claim(None)` 拒绝宣称 Agent 槽位容量，`guard_replicate_side_effect_package()` 永久拒绝
  复制副作用工作包提吞吐；`signals_from` 汇总四类信号，多机写入需求在单机 bench 里恒为 unknown。
  22 条命名用例 + 16 条漂移检查，手册见 `docs/spec-notes/p7-measurement.md`（§4 归因口径），
  台账见 `docs/reports/p7-baseline-<platform>.md`。
  **仍未取得**：`agent_slot_occupancy`（bench 内无真实 Agent 进程）、多机写入需求（需双机/容灾演练）。
  §3 决定记录在 `docs/reports/p7-decision.md`（含机器可读 JSON 块，由 `tests/p7_decision.rs` 解析成
  `GraduationDecision` 校验）：四类候选必须齐备且代价/负担/故障语义/恢复证据不留空，DAO 替换一律
  不算语义兼容，`record` 拒覆盖、`revise` 追加保留历史；决定里引用的每个小数与三位以上整数都必须
  在两份基线报告里逐字命中，报告表格值与详情段必须自洽——防手写估数与单点篡改。结论是**不迁移引擎**：
  只有单写争用在 Linux 越界（Busy 5），RTO 80 ms、体积 14.4 MiB、重建 229—390 ms 均无压力，
  多写者吞吐收益为负，故选「写入路径优化」方向，且本 Change 内不落地任何代码/数据变更。
  §4 预算冻结与出口（`cargo run -p xtask -- freeze [--out-dir DIR]` 连跑两轮同一测量集）：
  `compare_phases` 逐项算 `delta_pct`，容差 `PHASE_TOLERANCE_PCT=25.0` 闭区间；两轮环境或数据规模
  不一致整轮 `Incomparable` 且**任何项都不转**；只有「两阶段都 `Measured` + 在容差内」进
  `promotable()`，其余进 `unresolved()`；方法指纹（`FNV-1a(方法+samples)`）不等即归
  `method_mismatch`，口径或样本数一变就不许跨阶段比较；零基线记 `INFINITY`（零基线加噪声不算稳）。
  真实结论是**两平台都未冻结**（A2 加固后：Linux 可转 14 / 保留 12，Windows 可转 10 / 保留 16）。
  逐项定位：`idle_tick_cost`（0.05 ms 量级，接近计时器噪声）、`queue_selection_1000`、
  `write_contention_p99_growth`（Linux）；Windows 另加 `cold_start_to_api_ready`、
  `event_throughput_batched`、`index_rebuild_cost_100k_events`、`key_transaction_latency`、
  `web_projection_p95`——**不调宽容差、不删项、不改指纹造假**，
  改进口径或留 `Unverified`，两条都做不到就不冻结。冻结本身要过 `freeze_with_attribution` 四关：
  两轮可比、`relaxations` 为空（`guard_change_in_capacity_stage` 阻断降同步/放宽事务/跳门禁）、
  有可转项、每个排除项都先定位到「默认依赖」或「结构」；任一关不过就写「未冻结」并说明停在哪一步，
  不允许出现「空预算 + 已冻结」。冻结记录必须带 `BudgetScope`（版本/形态/环境/规模/容差），
  换任一项都要升版本重跑，Windows 数字不能当 Linux 预算、headless 预算不能约束桌面。
  预算与出口台账见 `docs/reports/p7-budget.md`、`docs/reports/p7-exit-evidence.md`，
  手册 §8 见 `docs/spec-notes/p7-measurement.md`；`tests/p7_freeze.rs` 10 条 + `tests/p7_exit.rs` 11 条守上述口径。
  **未取得**：`agent_slot_occupancy`、`state_to_web_visible_p95`、桌面四项、移动包体积、多机写入需求
  ——全部保持 `Unverified` 并写复测条件。`idle_cpu_ratio` 与 `cold_start_to_api_ready` 已在 Linux 实测，
  但前者只是 bench 内忙循环上界（127%，不等于守护进程 60 s 空闲占比）、后者不含会话握手且 Windows 不稳；
  `web_projection_p95` 只是本地事实装配，**不能顶替**渲染可见延迟。
- **真实演练工具包**（P2—P6）：`cargo run -p xtask -- drill --list|--template|--check|--render`。
  记录字段统一（case/phase/commit/operator/environment/steps/readings/gaps），`DrillRecord`
  **没有结论字段**、JSON 夹带 `conclusion`/`passed` 会被拒；`--check` 只报缺什么，全部填完也只写
  「记录完整」并固定输出「未结论」，判定留给出口评审。演练记录本身不解除任何门禁。
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
8. **禁止无证据引入重依赖**：新增默认依赖或一级对象须记录复杂度准入理由（§2.4）并锁定版本。当前新增依赖是 `serde`/`toml`（任务 1.2 配置 schema）、`rusqlite`（任务 2.6 单写事务，bundled 避免系统 SQLite）、`serde_json`（任务 5.1 执行协议 JSON 契约，需未知字段拒绝）、`schemars`（CR-004 从 DTO 派生 JSON Schema，ADR-0004）与 `tracing`/`tracing-subscriber`（CR-002 结构化日志，ADR-0006；不用 `env-filter`，避免 regex 进生产依赖树），理由与锁定版本见 `docs/spec-notes/p0-d0-config.md`、`docs/spec-notes/p0-execution-protocol.md`、`adr/0004-openapi-single-source.md`、`adr/0006-structured-logging.md`、`docs/compat/toolchain.md`；`bod-core` 仍保持零外部依赖。
9. **禁止把 Fake Executor 当生产能力**：它只用于故障 conformance，不写入支持矩阵 Supported。
10. **禁止以单平台绿灯代替跨平台通过**：`#[cfg(unix)]` / `#[cfg(windows)]` 分支，以及只在这些分支下
    编译的测试（如 `descriptor_file_is_owner_only`、`diff_gate_detects_real_symlink_escape`），
    在另一平台上是**空白**，不是通过。P2 僵尸缺陷即此例：`kill -0` 把僵尸判为存活，
    Windows 走 `OpenProcess` 永远测不出来。是否必须跑第二个平台由
    `cargo run -p xtask -- cross` 机械判定，不得目测、凭印象或为省事放宽。
11. **禁止手写 JSON 字符串转义**：一律走 `bod_app::json_text::json_string`。
    手写实现只处理 `\\`、`"`、`\n` 一类常见字符，漏掉 `\r`、`\t` 与其余 U+0000—U+001F 控制字符，
    而 RFC 8259 §7 要求全部转义——`--json` 输出里出现裸 `\r` 就是非法 JSON，且接收方是机器。
    这个缺陷曾以几乎逐字相同的代码存在三份（`main.rs`、`smoke.rs`、`conformance_report.rs`），
    正是「复制粘贴各改一份」造成的；`crates/bod-app/tests/json_literal_guard.rs` 会拦住第四份。
    例外只有 `canonical_json.rs`：规范编码需要字节级控制，必须自带转义，理由见该模块文档注释。
12. **禁止控制面请求绕过 Host/Origin 校验**：`HostServer::handle` 在路由与鉴权之前统一校验
    （`crates/bod-app/src/host_guard.rs`），新增路由**不得**另开入口或跳过这一步。
    Host 必须恰有一个且为回环名 + 实际监听端口，Origin 若存在必须命中白名单（Tauri 两种来源，
    dev 来源需显式 `allow_origin`）。`null` Origin 一律拒。这条不是可选的加固：
    `POST /_bod/handshake` 走描述文件令牌而非 Bearer，Host 校验是那条路径上唯一的网络层防线
    （DNS rebinding 场景）。
13. **禁止在 `handle` 里直接按路径分支**：新增 HTTP 端点一律加进路由表，并**必须**声明
    `auth`、`note` 与 `contract.status`（`crates/bod-app/src/http_router.rs`）。
    表有两张，按前缀分流（`host_handshake::handle`），**不按路径逐条判断**：
    - 控制面 `/_bod/*` → `host_handshake::ROUTES`（本机会话密钥、Host/Origin 校验）；
    - 产品接口 `/api/v1/*` → `delivery_api::routes`（§34.12 主体与目标判定，尚为 planned）。
    `auth` 与 `status` 无 `Default`，漏写编译不过；`note` 为空则测试不过。
    理由：CR-003 之前是 `match (method, path)` 链，加到第 5 个分支时「谁能调、拒绝答什么码」
    只能靠读每个分支的代码才知道，而 §6/§11 合计还要再加 10 个业务接口。
    拒绝码按端点保持稳定（`BOD_SHUTDOWN_DENIED` 不等于通用 `BOD_UNAUTHORIZED`），
    客户端按码分支，改码等于改契约。
14. **禁止让参数路由吃掉已被静态路径占用的路径**：`match_route` 的判定顺序是
    「路径归属优先于方法」——静态模式一旦匹配该路径，方法不符就报 `MethodMismatch`，
    不再尝试参数模式。否则 `POST /delivery-tracking/imports` 与
    `GET /delivery-tracking/{id}` 并存时，`GET /delivery-tracking/imports` 会被解析成
    「查 id=imports」，**把方法错误伪装成数据不存在**，客户端会去查数据而不是改方法。
    新增端点时必须检查是否存在同名的静态子资源与参数路由竞争；
    `static_path_owned_by_another_method_is_not_captured_by_parameter_route` 锁这个行为。
15. **禁止新增零引用模块而不登记**：新模块要么有生产调用方，要么在支持矩阵显式登记
    `integration: planned`（写明目标切片）。`p4_coverage.rs` 那种「断言模块已在 lib.rs 注册」
    的漂移检查**不算调用方**——它让 1,424 行的 `coverage_receipt` 在门禁全绿的同时零引用。
    检查用 `cargo run -p xtask -- reach --check verification/reach-baseline.tsv`：
    只对**相对基线新增**的不可达模块报警，既有孤岛不返红（它们是待接线资产或待判定沉淀）。
16. **禁止手写 OpenAPI 文档或第二份路径表**：契约从路由表投影
    （`crates/bod-app/src/openapi.rs`，`document_many` 合并控制面与产品接口两张表），
    新增端点必须同时写 `contract`（summary / status / request / query / responses），
    漏写编译不过。三条绝对不能手写：
    - **401**：拒绝码只能来自 `Auth::Bearer(code)` / `Auth::InHandler(code)`，
      在 `contract.responses` 里再写一条 401 会被 `openapi::document` 直接拒绝；
    - **404**：分发层对所有未匹配请求都答它，由投影统一添加；
    - **组件**：嵌套类型的 `$defs` 由 `register_body` 自动提升成 `components.schemas`
      并改写引用前缀，别手写 schema、也别加 `#[schemars(inline)]` 复制内容。
    `contract.status` 区分「路径已冻结」与「已经能用」：`planned` 必须声明 501 且 handler
    只能是 `not_implemented`，`served` 不得声明 501；实现落地要同时改状态、删 501 响应，
    测试双向断言（`planned_operations_are_marked_and_explain_the_501`）。
    改 DTO 或路由后必须重生成 `verification/openapi.json`
    （`BOD_BLESS_OPENAPI=1 cargo test -p bod-app --test openapi_contract`）并与代码一起提交，
    否则 `checked_in_document_matches_generated_one` 会红。
17. **禁止把凭据写进日志或审计**：控制面日志字段固定为
    `correlation_id`/method/path/status/outcome/耗时，请求头、请求体与查询串一律不进日志
    （那里恰好有握手令牌与 Bearer 密钥经过）。审计的 `basis`/`reason` 由
    `AuditEvent::validate` 拦凭据形态，库层只兜「决策与原因不一致」这一条，
    拦不住令牌文本——**别以为有库约束就够了**。要新增日志字段，先回答
    「这个字段会不会把令牌带进来」。
18. **禁止在观测报表里把「算不出来」写成 0**：`observability::snapshot` 的
    `not_measured` 必须如实列出缺数据的项与原因。报表里的 0 会被读成「测到了，且是零」，
    比留空更危险。要新增指标，先确认它的数据源是**已落库的事实**，不是内存态。
19. **禁止在交付跟踪里另立覆盖判定，或把覆盖结论存进库**：§11 的两条入口都只能把事实
    规范化成 `CoverageInput`，交给 `coverage_receipt::judge_coverage`
    （`crates/bod-app/src/delivery_tracking.rs` 只做规范化与投影）。表里放事实，
    结论读时算——存一份结论就一定会与事实漂移，而漂移后没人知道该信哪个。
    `delivery_observations` 是追加式历史：回滚是**新的观察**，UPDATE/DELETE 由 trigger 拒绝；
    人工补证缺授权范围与复验依据（`note` 为空）不得入库。
    视图里的 `allowed_actions` 只列**已实现**端点对应的动作（`delivery_api::is_served_action`），
    不许让视图承诺一个会回 501 的动作。
20. **禁止用 URL 或「看起来像 ID」的字符串充当来源身份**：身份只能是五元组
    （Company + Project + 连接实例 + 对象类型 + 提供方稳定对象 ID，
    `crates/bod-app/src/delivery_source.rs`）。只有定位时走候选，由负责人确认后才成为身份；
    确认是一次动作两个写入，必须带 `SAVEPOINT`（`UnitOfWork::confirm_source_candidate`）。
    来源修订按 `(version, digest)` 指纹幂等——**指纹不得含观察时间**，否则重复上报会变成新修订，
    「同一事件只确认一次」就不成立；同一标签解析出两个摘要必须留两条（§11 可变标签）。
    改这张表或加列前先看 `017_delivery_sources.sql` 的手记：`open` 每次都会重放全部迁移脚本，
    脚本必须天然幂等，`ALTER TABLE ADD COLUMN` 不幂等。
21. **禁止把「命令重放」与「对象重复」合成一个键**：两种去重是两件事，混用会同时产生
    「重放被当成新导入」与「不同对象被当成重放」（`crates/bod-app/src/delivery_idempotency.rs`）。
    幂等键的作用域是 `(company_id, idempotency_key)`，**不含命令**——一个键换命令也是冲突；
    载荷指纹由内容派生（不许调用方自编），同键异载荷一律拒绝。回执只写一次
    （`018_delivery_idempotency.sql` 的 trigger），且**必须与业务写入同事务**：
    失败不得留下「这个键已执行」，否则改正载荷重试会撞上自己伪造的冲突。
    对象去重走五元组 + `delivery_tracking_sources`（一个来源身份只承载一条跟踪关系），
    重复导入返回**已有关系**，不建第二个跟踪项。
22. **追加关联必须带 `expected_version`，且纯重复不得推进版本**
    （`crates/bod-app/src/delivery_link.rs`）。追加成功才 `resource_version + 1`：
    关系和目标都已存在时照样 +1，两个客户端会互相把对方的期望版本判过期，重试变成必冲突。
    期望版本不一致就**什么都不写**，不做事后合并。关系 ID 与目标 ID 由身份派生（长度前缀
    编码后求 sha256），不落库也不许字符串拼接——分段里的分隔符不能伪造出同一个 ID。
    目标身份不 trim（`"prod "` 是另一条客户记录），但 `None` 与 `Some("")` 都是「整个环境」。
    已关闭的跟踪项不再接受关联：写入口必须与 `allowed_actions` 同口径。

23. **禁止把新领域塞进 `store.rs` 主文件**
    （`crates/bod-app/src/store.rs` + `src/store/*.rs`）。主文件只放**跨领域**的东西：
    迁移表、连接与事务生命周期（`open` / `transaction` / `commit` / `rollback`）、
    `StoreError`、行结构、以及被多个领域共用的编码辅助（`deny_reason_str` 之类）。
    具体领域的读写放进同名子模块：`store/delivery.rs`、`store/audit.rs`、`store/queue.rs`、
    `store/budget.rs`、`store/execution.rs`、`store/artifact.rs`、`store/project.rs`、
    `store/governance.rs`、`store/maintenance.rs`、`store/rows.rs`。
    子模块用 `impl SqliteStore` / `impl<'a> UnitOfWork<'a>` 扩展同一对类型（私有字段对后代模块可见），
    文件头写清「本模块负责哪一域的不变量」。
    **禁止**再往主文件加 `pub fn`：主文件超过 600 行就该问一句「这是哪个域漏了」。
    新增领域 = 新增文件 + 在 `mod.rs` 注册 + 在 `pub use` 里按需重导出，
    不要靠「先写进 store.rs 回头再拆」——CR-005 就是那么涨到 8,588 行的。
    子模块**不需要**单独的可达性登记：`xtask reach` 按顶层模块归并，
    拆分文件不改变读数（`module_name` 只取 `src/` 之后的第一段）。

24. **禁止在证据冲突时自行选一份**
    （`crates/bod-app/src/delivery_tracking.rs`）。同一目标、同一 `revision` 上出现两个不同
    非空 `digest`，就是连接器与人工证据互相矛盾。此时结论必须停在 `PendingVerification`
    并说明冲突在哪一组版本上，**不得**按时间取最新那条当结论——那是替人做决定。
    判据必须带版本：只比摘要会把正常发版（新版本换内容）判成事故，「冲突」这个词就废了。
    两份观察仍原样留在 `evidence_stamps` 时间线里，「保留两者」不是合并成一条，
    也不是把其中一条标成「被取代」。

25. **审计的动作词表与授权的动作词表是两个闭集**
    （`crates/bod-app/src/audit_log.rs` 的 `AuditAction` vs `authorization::StageAction`）。
    阶段动作是**准入**词表，每个变体都必须能回答「需要什么凭据」；审计是**事实**词表，
    还要记准入之外的持久写入（`import-source` / `append-link`）。合成一个枚举只有两条路：
    给业务写入编一个假的 `StageRequirement`，或者这些动作永远进不了审计——两条都在说谎。
    映射只有一个方向（`From<StageAction>`），两个构造入口各挡错的那一类：
    阶段判定只能走 `AuditEvent::from_stage`，业务写入只能走 `AuditEvent::for_write`，
    互相喂错返回 `AuditRejection::ActionKindMismatch`。
    **`correlation_id` 不得拿幂等键顶替**：键是调用方自编的字符串，取个 `k-secret` 之类的名字
    就会撞上脱敏闸门（`SECRET_MARKERS` 含 `secret`），整条写入被拒——键名不该决定成败。
    审计与业务写入**同事务**：审计写不进去就整条回滚（有用例
    `import_rolls_back_when_the_audit_row_is_refused` 守着），不允许「业务成了、审计没有」。
    只有**真新增**才记审计：`appended=false` 的幂等空操作不写，否则「谁改了什么」凭空多一次改动。
    拒绝路径（如 `SOURCE_ALREADY_LINKED`）要留痕必须另起事务，目前**没做**，别当已完成。

26. **未实现的能力不得出现在支持矩阵的已实现项里，也不能沉默**
    （`docs/spec-notes/p0-support-matrix.md` + V2.9.9 技术规格 §12）。「AI 起草」在本版是
    未实现 Capability：矩阵必须显式写一行「未实现」，而不是留白——留白在评审眼里与
    「不需要」同义，在销售嘴里与「快好了」同义。判定结论永远由
    `coverage_receipt::judge_coverage` 给出，草稿器（规则摘要或将来的模型）只影响措辞；
    这条要靠回归用例守（`verdict_does_not_depend_on_any_drafter`），不能靠人工约定。
    同理：`StageFacts` 的装配失败必须返回「取不到事实」并记录原因，**不得**降级为
    「全部可见」或「全部不可见」——后者会把一个基础设施故障伪装成「你没有权限」。

27. **判定输入与措辞输入必须能机械分开，阶段拆分必须能分别归因**
    （`coverage_receipt::verdict_does_not_depend_on_any_drafter`、
    `authorization::visibility_and_target_granularity_fail_for_different_reasons`）。
    三条都靠结构而不是注释：
    - **哨兵要能挡住新增字段**。判「结论不依赖草稿器」时，把判定输入的字段**穷尽解构**
      （`let CoverageInput { tuple, deployment, .. 逐个列出 } = input`）；写成 `..` 或
      `input.population_known` 之类就没用了——`..` 会把将来新增的 `drafted_summary` 吸收掉，
      用例照样全绿而判定已经依赖了措辞。
    - **同一档对外码只能有一个来源**。`delivery_api::DENIED_CODE` 由
      `RecordRejection::ApiDenied.as_code()`（`const fn`）派生；字面量各写一遍，
      改档位时必有一处漏改，而这种漏改不会红。
    - **合成一步的判定说不清是哪条在挡**。可见性（有立场）与目标粒度（哪一个目标）分开写，
      否则「内部记录不该被客户看到」与「兄弟客户的记录不该被看到」在测试里只观察到一个「不可见」，
      将来放宽其中一条时无人能判断改对没有。
    `target_visible` 对「无客户维度的记录」返回 `true`（粒度不适用）由阶段二挡下，
    是刻意的：两阶段各自只回答自己的问题。

28. **授权事实只能按主体装配，取不到就报错，不许降级**
    （`crates/bod-app/src/store/facts.rs`、`store::facts::tests`）。
    `StageFactsPort` 是 §8.1 判定的唯一取数口，四条约束都是可机械判断的：
    - **按主体投影，不是公司全量**。`grants` / `project_members` / `customer_bindings`
      都带 `WHERE principal_id = ?`；判定只读传入主体的那几行，装全公司会把一次请求的
      成本绑到组织规模上。`release_operators` 是例外：判定问「我在不在名单里」，
      但直接把「我是否在」查成一个布尔会让 `StageFacts` 从事实退化成答案。
    - **空事实 ≠ 空判定**。缺表、锁超时、表外取值一律返回 `StageFactsError`
      （`BOD_FACTS_UNAVAILABLE` / 503）。返回空 `StageFacts` 会让 `record_visible` 全假，
      每个请求都 `BOD_NOT_FOUND`，且与「对象不存在」不可区分——比直接报故障更糟。
    - **同一枚举在库里有两种拼写**：`members.kind` 与 `project_members.kind` 是小写
      （`user` / `agent`），`issues.owner_kind` 是首字母大写（`User` / `Agent`）。
      跨表按文本比对会**恒不相等**，读侧必须各按写入侧还原。
    - **撤权修订只认 `authorization_revocations`**，不从 `grants.active = 0` 的行反推：
      `grants.revision` 是授予时的修订，停用不 bump，推出来偏旧，
      而 `check_subscription` 用 `since < revoked_at` 决定重同步——偏旧等于漏一次重同步。
      撤权表是追加式的（UPDATE/DELETE 由 trigger 拒绝），改写它等于追溯性地改掉
      「按哪一版授权判的」。
    `assignments` 从既有 `issues.owner_id / owner_kind / assignment_grant_revision` 派生，
    **不新建表**：那三列是写路径唯一维护的指派事实，再建一张表就是第二份会漂移的真相。
    代价是 `assignment_grant_revision` 为空时只能记 0（写路径不会产生，判定也不读它）。

29. **读没有动作；容器可见性有三档；装配缺失不许回退成匿名**
    （`crates/bod-app/src/delivery_api.rs`、`authorization::ContainerRead`、
    `store::read_port`）。三条都是这次接线时踩到的，判据都可机械复核：
    - **别为读请求扩 `StageAction`**。它的十个变体各自回答「改这件事需要什么凭据」，
      塞一个 `Read` 进去会立刻逼出「读取需要 Dispatch 凭据吗」这种没有答案的问题。
      读走 `read_container`：只判立场与目标粒度，**不查 `grants`**。
      代价是「看得见却做不了」这一档（`ScopeDenied`）只存在于命令路径——读路径上
      它与 `NotFound` 必须同答，否则返回码本身就成了存在性枚举工具。
    - **容器记录是三档，不是两档**。跟踪项这类容器不带客户维度，客户协作者对它的
      `is_visible` 恒为假。用布尔只有两种写法，两个都错：判「不可见」→ 客户永远读不到
      自己的目标；判「可见」→ 内部记录对所有客户敞开。正确结果是 `ByTarget`
      （按目标放行），且一条都不剩时必须回 `NotFound` 而不是 200 空 `targets`。
    - **装配缺失 = 拒绝，不是匿名**。`HostServer::delivery` 是
      `Option<(Arc<dyn TrackingRead>, DeliveryActor)>`——两个依赖合成一个 `Option`，
      半配置状态在类型上不合法。未装配时一律 401，且**对任何 id 同答**。
      不许为了「先跑起来」硬编码一个 `local-user`：匿名主体会让权限判定与审计归属
      同时失真，而失真后的记录比没有记录更难查。
    - **契约标 `served` ≠ 线上能读**。端点实现、判定接线、用例齐全就可以标 `served`；
      但如果没有任何生产入口调用装配入口，真实请求仍会 401。这种差距必须写进
      tasks.md 与报告，不得用「已 served」糊过去。

30. **撤权是「置 0 + 追加修订 + 同事务审计」，判据是「有没有生效授权」**
    （`store/revoke.rs`、`authorization::revoke_member`）。四条约束都能机械复核：
    - **判断该不该写，看「有没有生效授权」，不看「有没有撤权记录」**。撤权后又被
      重新授权时记录还在，这时必须能再撤一次并 bump 修订——否则离线客户端不会重同步。
      反过来，一行生效授权都没有时撤权是 **no-op**：不追加修订、不写审计。
      这与导入来源同一条纪律——**只记真发生的事**。
    - **修订号在事务内现算**（`MAX(revision) + 1`），不由调用方传入。填错会让
      `check_subscription` 的 `since < revoked_at` 漏一次重同步，而错值看起来是合法数字。
    - **停用而不删除**。四张事实表都有 `active` 列，撤权置 0。删除会让「这个人曾经
      在这个项目里」无从查证，重新授权还要从头插一遍。历史归属另有约束：
      **不改 `issues.owner_id`**（`history_rewritten` 恒 false），所以
      `StageRequirement::Work` 在撤权后**仍然放行**——`Work` 只看 `target.assignee`。
      「保留归属」不等于「保留执行权」，要收紧须让 `Work` 同时要求成员资格，
      那是判定语义变更，得由产品决定，不许在撤权写入里顺手改。
    - **审计 `basis` 的正文与动作前缀分开检查**。`basis` 形如 `<action>:<text>`，
      前缀是程序常量。整段过脱敏闸门会误伤：`revoke-authorization:` 里含
      `authorization:`（那条标记本是抓 HTTP 头的），结果是撤权审计永远写不进去，
      报错只说「basis 含凭据」。修法是只剥掉「本事件动作名 + 冒号」再查正文；
      剥不掉就整段查，防的是有人把 `authorization: Bearer x` 存进 basis 冒充前缀。

## 验证与完成规则

- 每个切片自带实现、测试与验证索引；不把测试路径存在当作测试通过。
- 依赖方向用 `cargo run -p xtask -- verify --stage P0` 校验，违例必须被阻断（可用 `--self-test` 证明守卫有效）。
- 编译检查：`cargo check --workspace --all-targets`，测试：`cargo test --workspace`。
- 里程碑出口：相关 I/G 门禁有真实证据后才勾选 tasks.md；每个阶段 Change 在自己出口证据齐全后归档，P0 已于 2026-11-19 归档，后续阶段不回溯修改 P0 历史约定。
- 工具链调用前先 `export PATH="$CARGO_HOME/bin:$PATH"`（详见 `docs/compat/toolchain.md`）。
- **架构决策必须落地**：实现与 V2.9.6 规格冲突（尤其 §3 技术栈）时，要么改代码，
  要么在 `adr/` 新增 ADR 记录「为什么推翻 + 代价 + 可机械判断的撤销条件」，
  两者必有其一。当前已记录：ADR-0001（保留同步传输栈，不引 Tokio/Axum）、
  ADR-0002（工作台暂不引前端框架）、ADR-0003（依赖方向用机械守卫）、ADR-0004（OpenAPI 单源：schemars 派生 + 从 `ROUTES` 投影）、
  ADR-0005（分层判据是「零外部依赖」）、ADR-0006（日志与审计分工）。
  `docs/` 被 gitignore，决策记录不得放在那里。
- **跨平台验证触发条件**：改完代码跑 `cargo run -p xtask -- cross`（默认比较 `HEAD~1..HEAD`；
  提交前用 `--worktree` 或 `--staged`）。退出码即结论：
  `0` 可以只跑单平台，`1` 必须跨平台，`2` 无法判定——**2 按必须跨平台处理，不得当作放行**。
  判据不硬编码文件清单，而是每次扫描源码里的 `cfg(unix)` / `cfg(windows)` / `cfg(target_os...)`
  等属性的实际位置，并额外把构建配置（`Cargo.toml`、`Cargo.lock`、`build.rs`、`rust-toolchain.toml`）
  与新增/删除的文件计入。它只回答「这次改动要不要在 Linux 再跑一遍全量」，
  不改变 `verify --stage P0` 的结论。
  注意 WSL 的边界：内核语义可信（`kill`、文件权限、symlink），但 `/mnt/e` 的文件系统与网络都是
  虚拟化的，不能当真实 Linux 服务器或 P6 2.8 双机证据。
- **模块可达性**：`cargo run -p xtask -- reach` 输出「可达 / 仅测试 / 孤岛」三态，
  口径是**全工作区**代码引用（`crates/*`、`apps/*`、`xtask/src`），字符串字面量与
  `pub mod` 声明不算引用，根文件的 `pub use` 只记 re-export 不计可达。
  它是读数不是门禁（成功即退 0）；`--check verification/reach-baseline.tsv` 才在
  「新增不可达 / 原可达退化」时退 1。行数按逻辑行计，末尾无换行的文件比 `wc -l` 多 1。
