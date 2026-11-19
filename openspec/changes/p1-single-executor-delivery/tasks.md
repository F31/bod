# Tasks: P1 单执行者交付

> 规格来源：本 Change 的 `specs/*/spec.md`。依赖 P0 Change 的可信核心；不实现第二执行端、负责人切换、影响图或远程执行。所有 I/G 绑定初始为 planned。

## 1. 工作区与 Git 基线

- [x] 1.1 实现每 Attempt 独立 worktree/分支与唯一写目录，验证 `bod-app::workspace_safety` 的 G-29：两个并行 Attempt 不共用暂存区、不读取对方未提交内容。
  - 证据（2026-11-19，本机 git 真实 worktree）：`cargo test -p bod-app workspace_safety` → 10 passed。`g_29_parallel_attempts_use_separate_indexes` 在临时仓库建 `bod/attempt-a` / `bod/attempt-b` 两个 worktree，A 写入并 `git add` 后：`changed_paths(a) == ["only-a.txt"]`、`changed_paths(b) == []`、B 目录不存在该文件、B 的 `git diff --cached` 为空。`WorkspaceManager::create` 的目录名经 `sanitize_attempt_id` 过滤（`attempt/1` → `attempt-1`，`..` 被拒）。G-29 已登记到 `verification/gates.yaml`（implemented）；`xtask verify --stage P0` 现报「索引中未被规格引用的条目 G-29」，因为 G-50 的引用来源仍是已归档 P0 的 proposal/tasks——P1 出口（5.3）把来源切到 P1 的 proposal/tasks 时，I-10/I-17/I-26/I-36/I-55 与 G-11/G-15/G-16/G-51 必须同时登记，否则 G-50 会如实失败。
- [x] 1.2 实现 write_scope/coordinated_scope 声明与提交 diff 门禁，拒绝软链接穿越、Git 管理目录与未授权共享配置，验证越界候选进入 NeedsRevision 且列出具体路径。
  - 证据：`diff_gate_rejects_out_of_scope_and_shared_paths` 一次提交 7 条路径，只有授权路径通过，其余进入 `NeedsRevision` 并按原样列出（`workspace_safety_extra.rs` 证明段边界匹配不放行前缀同名文件；`Cargo.toml` 报 `SharedUngranted`；`.git/config` 报 `GitMetadata`；`../outside.txt` 报 `Traversal`；`/etc/passwd` 报 `Absolute`）。`diff_gate_accepts_in_scope_and_granted_shared_paths` 证明 `coordinated` 显式授权后 migrations 目录可写。软链接穿越用可注入的 `PathInspector` 断言（Windows 默认权限建不了符号链接），另有 `#[cfg(unix)]` 的真实 symlink 用例。
- [x] 1.3 实现共享资源协调清单（公共类型/API schema/migrations/根依赖/OpenSpec），验证同一 migration ID 冲突被阻断且按稳定 ID 分配。
  - 证据：`SharedLedger` 按 `(kind, value)` 唯一登记，跨工作包重复即返回含持有者的 `SharedConflict`；`next_migration_id()` 取已有最大数字 +1 三位补零，`claim_migration` 对同一 attempt 幂等（返回同一个号，不再领新号——修复前重试会把号段占掉，测试已覆盖）。`shared_ledger_blocks_duplicate_migration_ids` 还验证 `merge_gate` 一次列出两个工作包提交同一 `007` 的冲突。
- [x] 1.4 实现工作区生命周期与保留策略，终态后回收但保留脏文件/未确认现场引用，验证取消后用户仓库未被删除且可查询待核实项。
  - 证据：`cancel_with_dirty_files_retains_scene_and_keeps_repo` → `KeepDirty` 下返回 `Retained`，工作区仍在、`repo/.git` 未被删除、`SceneRegistry::pending_review()` 可查到该现场与 `dirty.txt`；空白确认走 `DiscardDirty` 被 `DirtyNotDisposed` 拒绝；给出确认后只移除 worktree，仓库与分支 `bod/attempt-cancel` 都保留；`resolve(Abandoned)` 后不再待核实。`clean_workspace_is_removed_without_scene` 覆盖干净回收不产生待核实项。
- [x] 1.5 实现测试端口、临时目录与构建输出隔离，验证两个工作区并发启动测试服务无端口冲突或跨 Company 数据读取。
  - 证据：`PortAllocator` 在 `41000..41002` 内顺序分配，两个 attempt 拿到不同端口、同 attempt 重复申请幂等、释放后可复用；`plan_env` 按 attempt 派生 `tmp/db/build` 三个独立路径，`isolation_violations` 对同端口/同目录/同 DB/同构建输出/共享缓存可写逐项判冲突。`port_range_exhaustion_is_reported` 覆盖区间用尽与非法区间。

## 2. Context 与 Profile

- [x] 2.1 实现 ContextBundle 清单与确定装配，覆盖 spec/profile/skill/preference/input/adapter/runtime/assembly 版本与 omitted_refs，验证 `bod-app::context_handoff` 的 `invariant_i_17`：相同输入生成相同清单与摘要。
  - 证据（2026-11-19，`cargo test -p bod-app context_handoff` → 12 passed）：`assemble` 冻结 spec/profile 修订、三类引用（`kind:id@revision#sha256:…`）、adapter 版本、`runtime_config_hash`、常量 `assembly_version=ctx-assembly/1`、`omitted_refs`、密钥引用、验收与 Question。`invariant_i_17_same_input_replays_identical_manifest` 断言重复装配得到 `PartialEq` 相等结构、相同 `digest()` 与 `manifest_text()`；把三类引用全部 reverse 后摘要不变（排序去重生效）；`profile_revision` 改一版即换 `context_bundle_id`。`assembly_rejects_mutable_revisions_and_empty_fields` 证明 `latest` 被 `MutableRevision` 拒绝、空 `attempt_id` 与越界权限快照分别报 `EmptyField`/`UnscopedPermission`。清单字段与派生规则见 `docs/spec-notes/p1-context.md` §2。
- [x] 2.2 实现按 revision/digest 的不可变片段缓存与权限复核，验证撤权后缓存命中被拒绝并重新装配（G-48 的同类断言）。
  - 证据：`FragmentCache` 键为 `kind:id@revision#digest`，`insert` 要求 `sha256(body)` 等于引用摘要，否则 `ContentDigestMismatch` 且不落条目（`cache_requires_matching_content_digest_and_new_keys_on_change`）；换内容必须换 digest，旧键保留、新键独立（`len == 2`），不覆盖旧内容。`permission_revocation_rejects_cache_hit_and_reassembles`：同权限命中并返回正文；`without_project` 撤权后 `lookup` 返回 `PermissionChanged`（不返回旧正文），重新装配时该引用进入 `omitted_refs`（`not_permitted`），清单摘要随权限变化。`cache_rejects_cross_company_reads` 覆盖跨 Company 拒绝。G-48 只覆盖隔离/撤权/清空，**容量与热点回源未实现**，故在 `verification/gates.yaml` 登记为 `unverified` 而非 implemented。
- [x] 2.3 实现指令优先级与冲突检测，外部网页/Issue/工具输出标不可信，验证工具输出要求改验收时形成 Question 且原验收不变。
  - 证据：层级固定为 `user_goal_and_acceptance > project_authorization > profile > preference > external_material`；`Instruction` 字段私有，外部来源只能经 `Instruction::external` 构造，非外部 `OriginKind` 报 `ExternalOriginRequired`，因此无法伪造 `Trusted`（`external_sources_are_untrusted_by_construction` 逐类断言 WebPage/IssueBody/ToolOutput/Feedback）。`instruction_priority_requires_question_not_silent_override` 三段：工具输出 → `acceptance` 与输入逐字相等、`applied` 为空、`RejectionReason::UntrustedSource` + 1 条 `trust=untrusted` 的 Question（`subject` 含工具名、`status=pending`）；项目规则 → `LowerPriorityLayer` + Question，验收仍不变；用户目标 → 追加一条验收、无 Question。`Constraint` 键本切片只标 `UnsupportedKey` 拒绝，留给 P2，不假装已协调。
- [x] 2.4 实现会话复用条件与角色变更重置，验证同一 Agent 跨两个项目的会话、工作区与密钥分离（`invariant_i_36`）。
  - 证据：`SessionRegistry` 按 `(agent, company, project)` 分区，复用要求 Company/Project/输入修订/Profile 修订/角色集/权限摘要/工作区/清单全同，否则按 `RoleChanged`/`ProfileChanged`/`AuthorizationChanged` 等新建并记录 `ResetEvent`（`from_session → to_session + reason`），被替换会话进入 `retired()` 且 `closed` 标原因。`session_reuse_requires_same_scope_and_role_change_resets` 断言同请求第二次为 `Reuse` 且 attempts 累积、换角色为 `New(RoleChanged)` 且 `reset_log` 可核对。`invariant_i_36_...` 断言两个项目的 session id、workspace id、ContextBundle id、`secret_namespace()` 两两不同，项目 B 的 attempts 不含项目 A 的 Attempt（历史不串用）。工作区由 `workspace_safety` 创建，本模块只校验对应关系，不宣称强隔离。
- [x] 2.5 记录上下文装配与缓存失效说明到 `docs/spec-notes/p1-context.md`，验证其中每个缓存对象与失效条件可追溯到本 Change 的 requirement。
  - 证据：文档 §3 以表格逐行给出唯一缓存对象 `CachedFragment` 的键、命中前三项复核（Company/Project/权限摘要）与三种拒绝条件，并标注追溯来源（`不可变修订复用` 场景「撤权后命中旧缓存」、G-48 同类断言）；§2 列出清单字段来源与排序规则，§4/§5 给出优先级与会话重置换算，§7 明列 5 项未覆盖（容量/热点回源、落表与重启查询、真实 Agent 接入、强隔离、Constraint 键协调），§8 给出 12 个用例与任务的对应表。

## 3. 讨论与澄清

- [x] 3.1 实现 Issue 讨论记录（author/时间/关联修订/来源与编辑历史），验证评论编辑保留原文与修订。
  - 证据（2026-11-19/20，`cargo test -p bod-app clarity_dialogue` → 7 passed）：`Comment` 记录 author、created_at、来源（host/im/web/external_api/agent_output）、目标 `{object_id, revision}`；`edit` 把旧正文压进 `history`（`edited_by`/`edited_at`），当前正文换新值，`target` 不变；正文相同的编辑不产生历史。`comment_edit_keeps_original_body_and_revision` 两次编辑后历史 2 条且顺序为「初版 → 修订」，`target.revision` 仍为 `spec-r7`；审计条目只含元数据与 `sha256(body)`，`audit_line()` 断言不含任何正文片段（对应「评论正文与审计摘要分开存储」）。
- [x] 3.2 实现评论无副作用与 Question→NeedsInput，验证 `invariant_i_38`：LGTM 不结单、不派发、不授权，Question 记录答复主体与重新唤醒条件。
  - 证据：唯一事件→命令映射 `commands_for`：`Comment`/`UnverifiedMaterial` 恒为空，`Answer` 只在已记录答复且答复人合规时给 `WakeAttempt`，`Converted` 只给「开启接受/反馈流程」而非接受本身。`invariant_i_38_comment_never_creates_control_effect` 对 LGTM/同意/approved/+1/批准 逐条断言 `signal=ApprovalLike` 但 `commands` 为空、`converted()` 为空、`acceptance_revision` 不变；信号分类只用于界面提示，不进入授权。`ClarificationQuestion` 绑定 attempt 与确定 input 修订，记录 `answerers`、`BlockedScope`、`WakeCondition`；`question_sets_needs_input_and_timeout_stays_pending` 断言 NeedsInput 三要素齐全、`task-migrate` 被挡而 `task-unrelated` 不被挡、超时只增 `timeout_notices` 且状态保持 `pending`/命令为空（不猜答案、不放行）、非指定答复人被拒、指定答复人答复只产生 `WakeAttempt`。`cancelled_question_leaves_needs_input_without_commands` 覆盖撤销退出 NeedsInput 且不可再答复。
- [x] 3.3 实现有权限的评论→Decision/FeedbackProposal 显式转换与外部回调认证，验证伪造批准回调不产生接受/发布效果，转换后决策绑定对象与修订。
  - 证据：`convert` 前置四检——`base_revision` 必须是确定修订（`latest` → `IndefiniteRevision`）、对象一致（`ObjectMismatch`）、修订一致（`RevisionMismatch`）、持有该 kind 的授权（`NotAuthorized`）；结果 `ConvertedDecision{kind, object_id, base_revision, source_comment_id, decided_by, grant_basis, status=proposed}`，`acceptance_revision` 不变。`explicit_conversion_binds_object_revision_and_grant` 覆盖 Decision/ScopeDecision/FeedbackProposal 三种绑定（命令分别为 `BeginAcceptanceFlow`/`BeginFeedbackReview`）、无授权时记录数不变、修订不一致与可变指针均被拒。`forged_approval_callback_only_registers_untrusted_material`：未认证回调（自称「客户已批准」）只登记为 `UnverifiedMaterial`（`trust=untrusted`、`reason=unauthenticated`），`comments()`/`converted()` 为空、`commands_for` 为空、验收不变；已认证但无发言权 → `Rejected(NotAuthorized)`；已认证且有权限 → 只成为普通评论且不自动转决策。`callback_rejections_are_explicit` 覆盖 MissingSubject/ObjectMismatch/IndefiniteRevision。
- [x] 3.4 编写澄清状态与超时处理说明到 `docs/spec-notes/p1-clarity.md`，验证每个稳定状态与恢复条件对应具体 scenario。
  - 证据：文档 §4 给出 `pending / answered / cancelled` 三态表（进入条件、允许动作、退出条件）并逐行标注对应场景「执行端请求澄清」与「LGTM 不结单」；超时行为单列（只记提示、保持 pending、零命令），§3 给出事件→命令映射表，§5/§6 给出转换四检与回调五步判定；§7 明列 6 项未覆盖（持久化、评论去重、NeedsInput 未写入 Condition、接受流程未接线、评论权限仅布尔入参、撤回/删除未实现）。

## 4. 直派闭环与工作台

- [ ] 4.1 实现首次使用路径与自动最小工作约定，验证无 Team/WorkPackage/Run 配置即可完成一次真实仓库指派（`invariant_i_10`）。
- [ ] 4.2 打通真实仓库实现→本地验证→人工/预授权接受闭环，验证 `bod-app::coding_integration` 的 `invariant_i_18`、`invariant_i_20`：接受绑定确定提交/规格/报告，桌面重启后仍可查询。
- [ ] 4.3 实现取消请求/确认停止分离与 Unknown 对账入口，验证 cancel_ack 不释放占用、不新建 Attempt，界面显示待核实与处理人（`invariant_i_11`）。
- [ ] 4.4 实现本地验证报告绑定与备份核验，验证自报通过但无报告时验收保持 Pending/NeedsRevalidation，备份缺 WAL/CAS 引用不标可恢复（G-6、G-9）。
- [ ] 4.5 实现工作台首页四问与任务页证据视图（只投影宿主事实、无第二 DB），验证待决定事项含阻塞主体、所需决定与可采取动作，任务页显示拒绝原因与证据（`invariant_i_55`）。
- [ ] 4.6 实现预算耗尽时阻止新派发并保留已接受成果，验证 InsufficientBudget 稳定状态与批准入口可见且不创建 Binding（P0 G-45 回归）。
- [ ] 4.7 编写 P1 首次使用与交付操作手册到 `docs/spec-notes/p1-user-flow.md`，验证手册每一步都能在工作台复现且标注了 Unknown/取消的诚实表达。

## 5. P1 验证与出口

- [ ] 5.1 运行 `bod-app::context_handoff`、`bod-app::workspace_safety`、`bod-app::coding_integration`、`bod-app::resource_state` 的 P1 用例，验证 G-11、G-15、G-16、G-29 均有命名 case 与真实断言。
- [ ] 5.2 在同一真实仓库与一种已接入 Agent 上完成一次完整交付演示，验证任务、验证、接受、备份四段证据绑定同一修订且报告注明 commit/环境。
- [ ] 5.3 汇总 P1 出口证据与支持矩阵，验证「无 Team/Run 配置完成真实任务」「取消与 Unknown 如实显示」两项有可核对报告，未测项保持 Target/Unverified。

## Workflow follow-up

- 评审通过后归档本 Change，并把 4 份 delta 同步到 `openspec/specs/`；归档前不把 P1 能力标为 Supported。
- 后续 P2—P7 以新 Change 承接，不修改本 Change 的历史约定。
