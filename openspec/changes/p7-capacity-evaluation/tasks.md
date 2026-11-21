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

- [ ] 2.1 实测恢复 RTO 与重启 ready，验证十万事件样本下的结果可复核且区分外部恢复耗时。
- [ ] 2.2 实测单写争用与 Busy/写等待，验证事务 p99 升高时不被误判为写锁争用，unknown 区域明确记录。
- [ ] 2.3 实测表/索引体积、查询与重建成本，验证数据体积压力与索引优化空间分别有数据和命令。
- [ ] 2.4 实测 Agent 与执行槽位占用、并发容量，验证扩容收益与执行节点扩展需求分别判断，不为提高吞吐复制有副作用工作包。
- [ ] 2.5 汇总四类毕业信号，验证没有以日均事件数或单一 RTO 指标推出迁移结论（§40.4）。

## 3. 方案对比与决定

- [ ] 3.1 对比索引/批处理/存储优化、执行节点扩展、PostgreSQL、外部持久执行服务，验证每项有迁移代价、运维负担、故障语义与恢复证据。
- [ ] 3.2 验证 DAO 替换不被当作语义兼容，验证候选迁移方案保留单写次序、真实并发测试与备份恢复演练要求（§11.7）。
- [ ] 3.3 形成 GraduationDecision 记录（瓶颈、样本、环境、对比、主体、回滚路径、未覆盖条件、复测触发），验证可审计且历史依据不随人员变化改写。
- [ ] 3.4 验证容量评估未改变业务语义或数据；任何引擎变更被阻断并要求独立 Change 与迁移门禁。
- [ ] 3.5 验证支持矩阵未被性能结论自动扩大，未测平台/协议/能力保持 Planned/Experimental/Unsupported（G-26 回归）。

## 4. 预算冻结与出口

- [ ] 4.1 连续两个可比较阶段执行同一测量集，验证差异在记录范围内，达标项才从 Target 转 Measured。
- [ ] 4.2 冻结正式预算并记录版本/适用范围，验证超标项先定位默认依赖与结构，且没有通过降低 SQLite 同步、事务、授权或接受门禁达标。
- [ ] 4.3 汇总 P7 出口证据，验证「明确瓶颈及方案对比」「不按日事件数机械迁移」有可核对报告，未测项保留 Target/Unverified 与复测条件。

## Workflow follow-up

- 评审通过后归档本 Change；若决定迁移引擎，另建 Change 并带迁移/恢复/并发证据，不修改本 Change 的历史决定。
- P0—P7 全部 Change 归档后，统一核对现行 `openspec/specs/` 与验证索引的一致性。
