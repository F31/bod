# Tasks: P7 容量评估

> 规格来源：本 Change 的 `specs/*/spec.md`。本阶段产出实测报告与决定记录，不改变业务语义；任何引擎迁移必须另建 Change。所有输出初始为 Target/Unverified。

## 1. 测量基线与工具

- [x] 1.1 实现 `xtask bench`/诊断命令与样本生成器，记录 OS/CPU/Rust/构建参数/SQLite 同步策略/数据规模/缓存冷热，验证另一次运行能以相同条件复现并列出差异项。
      → `cargo run -p xtask -- bench [--out PATH]`；`perf_probe::environment` 七类字段 + `EnvironmentRecord::diff` 逐字段对照（`note` 不参与比较），差异项写进报告抬头；用例 `task_1_1_environment_diff_lists_reproducibility_gaps`、`task_1_1_measurement_conditions_cover_every_required_field`、`task_1_1_environment_record_is_complete`。
- [x] 1.2 建立 headless、可选 Web、Tauri 桌面与移动发布的分形态报告，验证 Web 宿主开销不冒充桌面总内存、headless 数字不直接套用桌面。
      → `FormFactor` 四形态 + `by_form_factor`/`may_claim_form_factor`/`guard_form_factor`；Web/桌面/移动项全部 `Unverified` 并写明缺口；用例 `task_1_2_headless_numbers_never_stand_in_for_desktop`、`task_1_2_gap_items_are_all_unverified_and_per_form_factor`、`task_1_2_manual_declares_per_form_factor_reporting`。
- [x] 1.3 实现测量项集合（空闲 RSS、冷启动、事务 p99、事件吞吐、候选选择 p95/p99、Web 可见、十万事件重启、包体积），验证每项含样本数、命令、环境与失败原因且不把吞吐与尾延迟合并宣称达标（§38.2）。
      → 18 项，见 `docs/spec-notes/p7-measurement.md` §3；吞吐拆 `event_throughput_batched`（每批 fsync）与 `event_throughput_durable`（每条 fsync），`may_claim_all_targets` 任一未达即 false、`unresolved()` 点名；用例 `task_1_3_every_item_carries_samples_command_and_failure_reason`、`task_1_3_throughput_and_tail_latency_are_separate_items`、`task_1_3_rendered_report_carries_command_samples_and_status_per_item`、`task_1_3_manual_inventory_matches_probe_inventory`。
- [x] 1.4 实现报告归档与状态字段（Target/Measured/Unverified），验证缺样本或命令的报告不进入容量决策且不被记为达标。
      → `MeasureStatus` 三档 + `usable_for_decision()`/`may_claim_target()`；`Target` 项让整份报告退出决策；`bench` 退出码反映报告可用性（漂移信号，不是性能结论）；用例 `task_1_4_incomplete_reports_never_enter_capacity_decisions`、`task_1_4_state_machine_is_three_valued_and_blocks_unmeasured_items`。
- [x] 1.5 编写 P7 测量手册到 `docs/spec-notes/p7-measurement.md`，验证手册中每个目标值标为待实测而非承诺。
      → 手册 §3 目标列整体标 `Target（待实测确认）`，每格要么是数值目标要么声明"无冻结目标"；§6 逐条列未取得项与前提；用例 `task_1_4_every_target_in_manual_is_a_pending_measurement_not_a_promise`、`task_1_5_manual_is_linked_from_tasks_and_agents`。


## 2. 瓶颈定位

- [x] 2.1 实测恢复 RTO 与重启 ready，验证十万事件样本下的结果可复核且区分外部恢复耗时。
      → `recovery_rto_100k_events`（备份 → 复制到新路径=外部 → 重开 → `quick_check`+行数校验=内部，3 轮取中位数）+ `bottleneck::classify_recovery`（缺外部或缺内部一律 Unverified）；用例 `task_2_1_recovery_separates_external_from_internal`、`task_2_1_recovery_items_expose_external_and_internal_phases`、`task_2_1_to_2_4_missing_samples_never_produce_a_healthy_verdict`。
- [x] 2.2 实测单写争用与 Busy/写等待，验证事务 p99 升高时不被误判为写锁争用，unknown 区域明确记录。
      → `write_contention_p99_growth`（1/2/4/8 个独立连接各 40 次写事务，数 `DatabaseBusy`）+ `bottleneck::classify_contention`：无 Busy 且增长 >25% 只判 `QueueingWithoutBusy` 并列出 unknown 区域；实测 Busy 恒为 0（busy_timeout 默认值下连接是等待而非报错），手册 §4.3 写明「Busy=0 不等于无争用」；用例 `task_2_2_tail_latency_without_busy_is_not_write_lock_contention`、`task_2_2_contention_item_separates_busy_from_queueing`。
- [x] 2.3 实测表/索引体积、查询与重建成本，验证数据体积压力与索引优化空间分别有数据和命令。
      → `table_index_volume_100k_events`（库字节 + page_count×page_size + `dbstat` 索引页）、`index_rebuild_cost_100k_events`（真跑 `REINDEX`）、`indexed_vs_unindexed_lookup`（`payload LIKE` 全表扫 vs `event_id` 主键等值）；`classify_volume` 分别产出 size/rebuild/index_space 三个判断；用例 `task_2_3_volume_pressure_and_index_headroom_are_separate`、`task_2_3_volume_and_index_space_have_separate_items`。注：`events` 有 `UNIQUE(aggregate_id, aggregate_version)` 自动索引，不能用 `aggregate_id` 当「未索引」对照。
- [x] 2.4 实测 Agent 与执行槽位占用、并发容量，验证扩容收益与执行节点扩展需求分别判断，不为提高吞吐复制有副作用工作包。
      → `executor_slot_capacity`（单写 vs 8 写者吞吐）+ `classify_slots`（收益 <100% 判 `WritePathBound`，先优化存储）；`agent_slot_occupancy` 保持 `Unverified`（bench 内无真实 Agent 进程），`agent_slot_claim(None)` 拒绝宣称容量；`guard_replicate_side_effect_package()` 永久拒绝复制副作用工作包；用例 `task_2_4_scale_out_is_judged_separately_from_executor_demand`、`task_2_4_slot_items_and_guards_exist`。
- [x] 2.5 汇总四类毕业信号，验证没有以日均事件数或单一 RTO 指标推出迁移结论（§40.4）。
      → `bottleneck::signals_from` 产出 RTO/写争用/体积/多机四类信号，缺数据一律 `None`（unknown）；`graduation(&[], Some(n))` 直接 `RejectedByEventCount`；单机 bench 无法证明「确有第二写入者」→ 多机信号恒 unknown；用例 `task_2_5_event_count_alone_never_triggers_migration`、`task_2_5_no_migration_from_event_count_or_single_rto`、`task_2_5_missing_observations_stay_unknown`、`task_2_5_four_signals_are_aggregated_and_event_count_is_not_a_signal`。

## 3. 方案对比与决定

- [x] 3.1 对比索引/批处理/存储优化、执行节点扩展、PostgreSQL、外部持久执行服务，验证每项有迁移代价、运维负担、故障语义与恢复证据。
      → `docs/reports/p7-decision.md` §2 四类候选表 + `§3` 机器可读记录；`compare_candidates` 要求四类齐备且四项字段非空（缺项或留空即 `Incomplete`）；用例 `task_3_1_all_four_candidates_have_cost_burden_semantics_and_evidence`、`task_3_1_all_four_candidates_must_be_compared`。
- [x] 3.2 验证 DAO 替换不被当作语义兼容，验证候选迁移方案保留单写次序、真实并发测试与备份恢复演练要求（§11.7）。
      → `dao_swap_is_not_semantic_compat()` 恒拒绝；四类候选的 `failure_semantics` 均须交代单写次序；决定记录写明「真实并发测试 + 备份恢复演练」三条硬要求；用例 `task_3_2_candidates_keep_single_writer_order_and_real_drills`、`task_3_2_dao_swap_is_not_semantic_compat`。
- [x] 3.3 形成 GraduationDecision 记录（瓶颈、样本、环境、对比、主体、回滚路径、未覆盖条件、复测触发），验证可审计且历史依据不随人员变化改写。
      → `p7-decision.md` 的 JSON 块由 `tests/p7_decision.rs` 解析成 `GraduationDecision`；`audit_gaps()` 必须为空，`record` 拒绝覆盖、`revise` 追加并保留 `earliest`；引用的数字须在真实报告里逐字命中（防手写估数）；用例 `task_3_3_decision_is_auditable_and_append_only`、`task_3_3_decision_cites_real_reports_and_real_numbers`。
- [x] 3.4 验证容量评估未改变业务语义或数据；任何引擎变更被阻断并要求独立 Change 与迁移门禁。
      → `guard_change_in_capacity_stage` 阻断换引擎/降同步/放宽事务/跳门禁；`guard_replicate_side_effect_package` 阻断复制副作用工作包；决定记录声明「未改变业务语义或数据」且要求独立 Change；用例 `task_3_4_no_engine_change_inside_the_capacity_stage`、`task_3_4_engine_changes_are_blocked_inside_capacity_stage`。
- [x] 3.5 验证支持矩阵未被性能结论自动扩大，未测平台/协议/能力保持 Planned/Experimental/Unsupported（G-26 回归）。
      → `guard_support_promotion` 无 conformance/真机证据即拒绝；支持矩阵仍为 0 Supported；用例 `task_3_5_support_matrix_is_not_promoted_by_performance`、`task_3_5_performance_never_promotes_support_matrix`。

## 4. 预算冻结与出口

- [x] 4.1 连续两个可比较阶段执行同一测量集，验证差异在记录范围内，达标项才从 Target 转 Measured。
      → `xtask freeze [--out-dir DIR]` 连跑两轮同一测量集 → `capacity_evaluation::compare_phases`/`ItemDrift`/`PhaseComparison`（容差 `PHASE_TOLERANCE_PCT=25.0`，只有「两阶段都测到 + 在容差内」才进 `promotable()`）；真实结果：两平台两轮均可比（环境与规模一致），Windows 可转 10 项 / 超差 4 项，Linux 可转 7 项 / 超差 7 项，两平台都超差的 3 项为 `queue_selection_1000`、`recovery_rto_100k_events`、`idle_tick_cost`；报告 `docs/reports/p7-phase{1,2}-{windows,linux}.md` 与 `p7-two-phase-{windows,linux}.md`；用例 `tests/p7_freeze.rs` 六条（含容差闭区间、不可比两轮不转、无标量项不转、零基线不判稳）与 `tests/p7_exit.rs::task_4_1_*` 四条（表格判定与结论行一致、计数加总等于清单、每个数字能在阶段报告里找到）。
- [x] 4.2 冻结正式预算并记录版本/适用范围，验证超标项先定位默认依赖与结构，且没有通过降低 SQLite 同步、事务、授权或接受门禁达标。
      → `BudgetScope`（版本/形态/环境/规模/容差）、`DriftCause`（`DefaultDependency`/`Structure`/`Unknown`）、`DriftAttribution`、`freeze_with_attribution`（未定位即拒绝冻结、`relaxations` 非空即阻断）、`FrozenBudget`/`render_frozen_budget`；产出 `docs/reports/p7-budget.md`：**判定未冻结**（8 项排除，逐项给出默认依赖/结构定位 + 证据 + 复测条件），两平台都稳定的 6 项只记「可转 Measured / 可作预算输入」而不记「已生效」；未改任何正确性参数（`synchronous=Full`、`BEGIN IMMEDIATE`、`busy_timeout_ms=500` 均保持默认，`relaxations: []`）；用例 `tests/p7_freeze.rs::task_4_2_*` 五条 + `tests/p7_exit.rs::task_4_2_*` 两条。
- [x] 4.3 汇总 P7 出口证据，验证「明确瓶颈及方案对比」「不按日事件数机械迁移」有可核对报告，未测项保留 Target/Unverified 与复测条件。
      → `docs/reports/p7-exit-evidence.md`：结论 `done-with-unverified-items`；瓶颈表（单写 p99 5.31/1.68 ms、8 写者 182.37/338.57 ms、批量 58359—60619 events/s vs 逐条 377—1100 events/s、RTO 80 ms、库 14.4 MiB、重建 229—390 ms、多写者收益 −19%/−50%）逐行标注报告来源；四类候选对比指向 `p7-decision.md`；`graduation(&[], Some(n))` → `RejectedByEventCount` 与多机信号恒 `unknown` 写进 §2；9 项 `Unverified` 逐条给复测条件；并写明决定记录的 80 ms 出自基线轮、两阶段该项不稳定（57.669—125.842 ms），故只作结论依据不作预算数字；用例 `tests/p7_exit.rs::task_4_3_*` 四条。

## Workflow follow-up

- 评审通过后归档本 Change；若决定迁移引擎，另建 Change 并带迁移/恢复/并发证据，不修改本 Change 的历史决定。
- P0—P7 全部 Change 归档后，统一核对现行 `openspec/specs/` 与验证索引的一致性。
