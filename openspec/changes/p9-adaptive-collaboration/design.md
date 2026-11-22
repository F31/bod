# Design: P9 组织代次与缺口驱动的改组

## Context

动机见 `proposal.md`。约束来自四处，设计不得绕过：

- v1.0 附篇 §4 D4：每次认领包含 fencing token，旧持有者不能推进当前状态；改组须提交差量，
  标出继续、暂停、移交和新增的职责；重复触发且无有效变更时**保留原组织代次**。
- v1.0 技术 §2 D4：规划器产生工作包图差量，宿主按委任和组织代次提交；每次认领含 fencing token。
- v1.0 技术 §4.1：G/L/风险三轴独立，档位转换有明确退出边界，重复触发复用原决定。
- 仓库现状：`work_package_states` 已有 `version` 条件更新与租约；`audit_events` 已是追加式；
  `collaboration_plan` 有执行图准入但是 orphan；迁移 001–020 已应用，**任何已应用迁移不得原地改写**
  （`ChecksumDrift`）。

## Goals / Non-Goals

- 目标：缺口成为可去重、可追溯、可关闭的一等对象，且**关闭必须绑定解决它的证据修订**。
- 目标：组织代次单调递增，认领与写入按代次隔离；无有效差量不产生新代次。
- 目标：G0–G3 / L1–L3 可记录、可复核、可收缩，且不改变授权与预算的独立裁决。
- 目标：`collaboration_plan` 从 orphan 变为生产可达（被命令层引用）。
- 非目标：共源计数、影响重验索引、观测器暂停、离线核验、策略校准（见 `proposal.md` Non-Goals）。

## Decisions

### D1 缺口的身份用逻辑键，不用随机 ID

去重键：`(company_id, subject_kind, subject_id, rule_revision, trigger_source, window_key)`。
同一键重复触发**同一条**缺口，只前进 `last_seen_revision`，不新建行。

否决「用 uuid 主键 + 每次插入」：v1.0 要求「对同一目标、规则、触发源及观察窗口去重」，
并要求「暂停决定仅一次」。随机主键会让每次触发都生成新行，于是「仅一次」只能靠应用层再去重——
那就等于把唯一性约束降级成一段可能漏写的代码。

否决「缺口不落表，只在读取时计算」：缺口的价值是**触发改组**，而改组必须能说明
「哪条缺口、哪一版证据触发」。纯计算没有身份，事后无法把代次绑回触发源。

### D2 缺口是待办，不是事实；两者不合并

`delivery_observations` 记录「发生了什么」，`delivery_gaps` 记录「还缺什么才能下结论」。
一条观测可能解决一条缺口；但缺口不能在观测写入时被**自动**关闭——
关闭是一次判定，必须由判定层给出所依据的证据修订。

五态与允许的转换：
`待补证 → 待授权 | 待外部事实 | 已解决 | 被取代`；`待授权/待外部事实 → 已解决 | 被取代`。
**不允许** `已解决 → 待补证`（那是新缺口，用新行 + `supersedes` 指向旧行）。
「已解决」行必须同时写入 `resolved_by_revision`，否则回放时无法说明它为何关闭。

### D3 代次是单调列 + 事件，不是第二张「分工表」

- `work_package_states` 增 `generation`（默认 1，单调递增）。
- 认领时把 `generation` 一并写入租约；所有推进型写入带 `AND generation = ?`。
- 每次**有效**改组在 `organization_generations` 追加一行：触发缺口、候选与排除理由、
  旧/新职责、业务修订、授权与预算复核结果。

否决「新建 `assignments` 表表示当前分工」：`issues.owner_id / owner_kind /
assignment_grant_revision` 已是写路径唯一维护的指派事实，再建一张表就是第二份会漂移的真相
（与 P8 3.6 的否决理由同源）。

否决「代次只放内存」：v1.0 D06 要求「旧代次结果不能写入当前有效结果」，
这条在重启后仍须成立，因此代次必须与认领一起持久化。

### D4 档位不落独立轨迹表，复用审计

- 当前档位是 `work_package_states` 上的三列：`compute_tier`（G0–G3）、
  `autonomy_level`（L1–L3）、`risk_class`。
- 档位变化写 `audit_events`，动作 `ChangeComputeTier`，`basis` 记录触发事实与观察窗口，
  `reason` 记录退出条件。

否决「新建 `tier_transitions` 表」：档位变化是「谁在什么依据下改了什么」，
与审计的字段完全同构；为它单开一张表只会让「查一次档位历史」变成两次查询。

否决「档位做成纯配置」：v1.0 要求 G 由任务事实决定、L 由治理授权决定、风险单独记录。
配置只能提供**上限**（例如「本租户最高 G2」），实际档位必须按当前事实判定，
否则「简单输入直通」永远无法成立。

### D5 迟到隔离复用条件更新，不引入新锁

旧代次的回报走现有 `UPDATE ... WHERE version = ? AND generation = ?`。
不匹配时返回 `StaleGeneration`，由调用方决定「记为历史事实」还是「进对账」。
不新增锁、不新增队列——P0 的单写事务与条件更新已经足够表达这条不变量。

### D6 `collaboration_plan` 接线的入口只有一个

`collaboration_plan` 保持纯值（不写库）；新增的 `store::generations` 负责把它的
`PlanRevision` 差量落库。**只有命令层可以调用这一对**，读取路径不得触发代次变化。
这样「组织代次只因有效差量而变」在结构上只有一处可能违反。

## Migration Plan

1. `021_gaps_and_generations.sql`：`delivery_gaps`、`organization_generations`；
   `work_package_states` 增 `generation` / `compute_tier` / `autonomy_level` / `risk_class`
   四列（带默认值，旧行按 1 与 G1/L1/medium 回填并在注释中写明是**回填假设**）。
2. 不改 001–020 任何字节。表数断言随之从 64 更新。
3. 旧库验证：用一个 020 状态的库跑迁移，确认既有行可读、`generation=1`、代次表为空。

## Risks

- **回填档位是假设，不是事实。** 旧工作包没有档位记录，回填 G1/L1/medium 只是让列非空。
  风险：有人把回填值当历史结论。缓解：迁移注释 + `tasks.md` 明确「回填值不得作为验收证据」。
- **代次列与租约列可能不一致。** 若某条既有代码只校验 `version` 不校验 `generation`，
  旧代次仍能写入。缓解：`tasks` 里要求全仓审计所有推进型 UPDATE，并加一条「只用 version 的
  写入点」清单；清单不为空则不改完不算完成。
- **缺口去重键选错会导致「该合并的没合并」。** 缓解：`window_key` 由调用方给出并在用例中
  覆盖「同窗口重复触发」与「换窗口应新建」两侧。
