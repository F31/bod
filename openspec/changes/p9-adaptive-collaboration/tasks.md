# Tasks: P9 组织代次与缺口驱动的改组

> 规格来源：`docs/智能体协作与可验证交付规范-v1.0.md` 附篇 §4（D4/A01）、§二之四（F01–F04）；
> `docs/bod智能体协作与可验证交付技术架构规格-v1.0.md` §2（D4 表）、§3、§3.1、§4.1、§9（切片 2）。
> 全部条目**未开始**。门禁：D01、D06、D08、D09、P01、P03、A-T1、A-T2。
> 计划路径不得当作证据；勾选必须附「跑了什么命令、得到什么读数」。

## 1. 盘点与冻结

- [ ] 1.1 盘点**所有推进型写入点**：全仓 `UPDATE ... WHERE` 中，哪些只校验 `version`、
      哪些已带租约/令牌。产出清单（文件:行 + 现状 + 是否需加 `generation`）。
      **这不是可选项**：design 的 Risk 2 要求清单为空才算改完；清单若含跨域写入（如 `store/delivery.rs`
      的版本推进），必须在 3.x 一并加上代次条件。
- [ ] 1.2 冻结缺口去重键与五态：把 `(company, subject_kind, subject_id, rule_revision,
      trigger_source, window_key)` 写进 `design.md` 的实现注记，并逐条对应 v1.0 D4「按触发源与观察窗口去重」。
      同时冻结「已解决必须有 `resolved_by_revision`」这条。
- [ ] 1.3 冻结档位语义三轴：G 由任务事实、L 由治理授权、风险独立记录；配置只能给上限。
      把技术 §4.1 的五条转换逐条抄成枚举，作为 4.1 的输入（不是自由文本）。

## 2. 缺口对象（`adaptive-collaboration` 第 1 组）

- [ ] 2.1 迁移 `021_gaps_and_generations.sql`：`delivery_gaps`（逻辑键唯一索引、
      `state` CHECK 五态、`resolved_by_revision` 可空、`supersedes` 自引用可空、追加式 trigger 拒 DELETE）。
      **不改 001–020 任何字节**；表数断言从 64 更新，并在断言消息里写明加的是哪一类。
- [ ] 2.2 `store/gaps.rs`：`open_or_touch`（同键只前进 `last_seen_revision`，不新建行）、
      `resolve`（写 `resolved_by_revision`，只在允许的转换上成功）、`supersede`、
      `list_open(company, subject)`。所有写入走 `UnitOfWork`，不另起事务。
- [ ] 2.3 用例 5 条：同键重复触发只一行；换 `window_key` 新建一行；非法转换（`已解决 → 待补证`）
      被拒；`resolve` 缺证据修订被拒；`supersede` 后旧行不可再 `resolve`。
- [ ] 2.4 旧库验证：用 020 状态的库跑迁移，确认既有行可读、`generation=1`、两张新表为空；
      产出一条可复现命令写进本文件。

## 3. 组织代次（第 2 组）

- [ ] 3.1 `work_package_states` 增 `generation`（默认 1）+ 认领写入带代次；
      所有推进型 UPDATE 增加 `AND generation = ?`；不匹配返回 `StaleGeneration`（新 `StoreError` 变体）。
      完成判据：1.1 的清单逐行标注「已加 / 不适用（原因）」，**不允许遗留空项**。
- [ ] 3.2 `store/generations.rs`：`commit_regroup` 在单事务内写「缺口 → 候选与排除理由 →
      职责差量 → 授权与预算复核 → 新代次」，并更新 `work_package_states.generation`。
      **无有效差量时不加代次**（返回 `None`，不写行）。
- [ ] 3.3 用例 5 条：无差量不改代次且不写行；有差量则代次 +1 且差量可读回；
      旧代次回报被拒并返回 `StaleGeneration`；同缺口重复提交复用原代次（D09）；
      事务内授权复核失败整条回滚（不留下半份代次）。

## 4. 计算档位（第 3 组）

- [ ] 4.1 `bod-app::compute_tier`：`ComputeTier{G0..G3}`、`AutonomyLevel{L1..L3}`、
      `RiskClass`；`decide(&TierInput) -> TierDecision`，按技术 §4.1 五条转换给出
      「允许/维持/降档/待决」+ 退出边界。纯函数，不读写库、不读配置。
- [ ] 4.2 `work_package_states` 三列 + 档位变化写 `audit_events`（`ChangeComputeTier`）。
      审计 `basis` 必须含触发事实与观察窗口；缺任一项在写入前拒绝。
- [ ] 4.3 用例 5 条：简单确定性输入 → G0 且零模型路径；新反证 → 升档且记录触发；
      无新证据重复触发 → 不改档（复用原决定）；升档不增加预算额度（A01）；
      撤权即时生效且不受档位影响（A02 的「计算停止不构成接受」）。

## 5. 主动移交与接线（第 4 组）

- [ ] 5.1 `collaboration_plan` 从 orphan 转为生产可达：`store::generations` 引用它的
      `PlanRevision` 差量，命令层是唯一入口；读取路径不得触发代次变化。
      完成判据：`xtask reach --check` 里 `collaboration_plan` 不再是 orphan。
- [ ] 5.2 移交意图入库：字段按技术 §3.1（工作包、当前代次、预期修订、缺口、已完成事项、
      证据引用、未决副作用、建议能力、幂等键）；状态 `待评估 / 已采纳 / 已驳回 / 待决`；
      采纳时同事务更新职责、租约、预算预留与派发意图。
- [ ] 5.3 用例 4 条：无合格接手者 → `待决` 且保留未决责任（不静默丢弃）；
      采纳时费用留在原账目（预算不重置）；旧代次迟到回报只作历史（D06）；
      同一移交意图重放不产生第二次采纳（幂等）。

## 6. 门禁与验收

- [ ] 6.1 D01 / D06 / D09 端到端各一条：三条都必须同时断言「代次」「缺口状态」「迟到隔离」，
      缺任一项不算覆盖。
- [ ] 6.2 D08：同一目标分别以确定性输入与冲突新输入提交，前者 G0、后者升档并可收缩；
      档位轨迹从审计读回。
- [ ] 6.3 P01 / P03：候选不足与超出委任额度返回缺口且拒绝扩权扩额（不广播）；
      自审被排除、共享组织根的独立检查者按实际权限判定。
- [ ] 6.4 A-T1 / A-T2：单目标回滚只重验受影响目标；原责任人失权时只提交一个有效认领。
- [ ] 6.5 读数：`cargo test -j 2`（含 `-p xtask` 单跑）、clippy、fmt、`verify --stage P0`、
      `openspec validate --all --strict`、`reach --check`；跨平台由 `xtask cross` 判定后如实记录。
