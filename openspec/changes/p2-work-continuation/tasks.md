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
- [ ] 1.5 实现现场清单边界（submodule/LFS/锁文件/冲突/密钥过滤）与交接事件类型，验证 `invariant_i_47`：包内无凭据值，重复 accept 返回同一结果。

## 2. 接管事务

- [ ] 2.1 实现持久交接意图与分阶段状态机、expected_version 条件提交，验证等待总结期间重启不重复请求或创建 Attempt。
- [ ] 2.2 实现旧执行停止确认与隔离检查，验证 `invariant_i_48`：旧写入未停时新执行者写同一现场被拒绝，允许独立无冲突工作。
- [ ] 2.3 实现 effect_id/逻辑动作键对账与不可信 reconcile_hint 过滤，验证恶意 hint 中的脚本或 URL 不被执行且不明结果只阻塞相关依赖。
- [ ] 2.4 实现重建预检（摘要/授权/输入差异/文件/工具依赖）与 Question 生成，验证摘要不匹配时阻止自动继续。
- [ ] 2.5 实现接续可见性卡片（等级/保留成果/missing/停止状态/重建步骤），验证换人不改变验收或合并旧未验收成果。
- [ ] 2.6 编写接管事务与故障样本说明到 `docs/spec-notes/p2-takeover.md`，验证每个阶段对应 scenario 与可注入故障点。

## 3. 负责人切换

- [ ] 3.1 实现 Appointment/authority_epoch/scope/命令集/audience 绑定与当前状态复核，验证 `invariant_i_58`：旧 epoch 管理命令被拒且缓存可失效。
- [ ] 3.2 实现员工独立继续与执行/管理身份分离，验证切换时运行任务继续、已接受成果不重做，旧经理执行身份上传的合法成果保留（`invariant_i_59`）。
- [ ] 3.3 实现检查点增量读取与接管确认事务，验证只读确认不产生管理权、确认后命令仍逐条校验当前委任（`invariant_i_60`、G-42）。
- [ ] 3.4 实现历史成果/费用不可变与争议待核查标记，验证 `invariant_i_61`：新经理不能追溯修改旧接受或费用主体。
- [ ] 3.5 实现专项委任边界与组织元数据分离，验证项目负责人不能越过专项范围指挥，reportsTo 变更不迁移 Run/租约/权限（`invariant_i_62`—`invariant_i_64`）。
- [ ] 3.6 编写负责人切换操作说明到 `docs/spec-notes/p2-manager-transfer.md`，验证 §41.8 的每项验收要求都有对应命令与拒绝样本。

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
