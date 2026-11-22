# Tasks: P8 双入口交付

> 规格来源：本 Change 的 `specs/*/spec.md`（V2.9.8 §11）。全部条目**未开始**——
> 本文件建立时只完成了前置（CR-004 契约单源，ADR-0004），没有任何实现可勾选。
> 门禁绑定初始为 planned：`I-72`（两条入口同一结论）。计划路径不得当作证据。

## 1. 前置（已完成的相邻工作，不属本 Change 的实现）

- [x] 1.0 契约单源：`ROUTES` 即契约表 + `verification/openapi.json` 黄金比对（ADR-0004）。
      这是 §11「路径在 OpenAPI 冻结时定稿」的前置，已完成于 P8 之外。
- [x] 1.1 审计与观测底座：`audit_events`（015 迁移）+ `audit_log` 落库前校验 +
      `tracing` 结构化日志 + `observability` 只读报表（ADR-0006）。
      **只完成机制，审计当前没有生产写入者**——本 Change 的 3.5 与发送重验落库时
      必须调用 `UnitOfWork::append_audit_event`，并让 `correlation_id` 贯穿
      「草稿创建 → 发送时双点重验」两段，否则 §11 的「发送意图可在出站点核对」没有证据。

## 2. 盘点与冻结

- [x] 2.1 只读盘点：现有表/DAO/查询/UI 对内部 Attempt 外键与「内部创建来源」的依赖点，产出清单。
      结论写进 `design.md` 的 Migration Plan，不改代码。
      **结果**：50 张表、全库仅 1 条外键（`wake_receipts` → `work_queue`），无一条指向
      `attempts`/`runs`；唯一阻断点是 `acceptances` 的三个 `NOT NULL` 内部修订列——
      因此外部回执不复用该表，也不为它编造工作包修订（D3 的禁止路径）。
- [x] 2.2 冻结 §11 的 6 个操作进路由表（带 `contract`）并重生成 `verification/openapi.json`，
      验证 `openapi_contract` 全绿；此后路径不再改动。
      **结果**：新增 `delivery_api`（独立 `/api/v1` 表，与控制面 `/_bod/*` 分流），
      6 条路由全部 `x-bod-status=planned` 且声明 501；`openapi_contract` 11 例、
      `delivery_api` 4 例、宿主分发用例 1 例。**实现仍未落地**（3.x 起才写 handler）。

## 3. 跟踪项与来源身份（`delivery-tracking`）

- [x] 3.1 跟踪项模型：项目 × 客户目标 × 验收条件 × 负责人 × 缺口；允许内部 Run/Attempt/审批缺省。
      **结果**：`016_delivery_tracking.sql` 三张表（`delivery_tracking` / `delivery_targets` /
      `delivery_observations`），内部引用列全部可空；观察表追加式（trigger 拒绝 UPDATE/DELETE），
      人工补证空备注由 CHECK 与写入原语双重拒绝。读模型在 `delivery_tracking` 模块：
      覆盖结论由 `coverage_receipt::judge_coverage` 在读时算出，**库里不存结论**。
      生产入口是 `bod tracking --db <path> [--id <id>] [--json]`；
      `GET /api/v1/delivery-tracking/{id}` 仍是 `planned`（契约承诺按授权目标过滤，§34.12 未实现）。
      14 个单测 + 5 个 CLI 端到端用例。
      未做：来源身份五元组（3.2）、幂等与去重（3.3）、`expected_version` 追加（3.4）、
      导入端点（3.5）；摘要冲突判「待核实」需要来源身份区分「冲突」与「新版本取代」，留到 3.5。
- [x] 3.2 来源身份五元组与候选机制；内容更新追加来源修订（观察时间、摘要、操作者）。
      **结果**：`017_delivery_sources.sql` 四张表（`delivery_sources` / `delivery_source_candidates` /
      `delivery_source_revisions` / `delivery_observation_sources`）。身份唯一索引含 `project_id`，
      所以跨项目同名 PR 是两个来源；只有定位（URL/纯文本，含「把 URL 填进 ID 字段」）进候选，
      确认后才有身份；修订按 `(version, digest)` 指纹幂等，**不含观察时间**，因此重复上报只确认一次、
      同一标签两个摘要留两条。观察与修订用关联表连接（不能给 016 加列：迁移每次打开都会重放，
      `ALTER TABLE ADD COLUMN` 不幂等）。生产入口是只读的 `bod sources --db <path> [--project]
      [--candidates] [--json]`；候选确认写入口仍等 §34.12 主体判定。
      13 个模块单测 + 7 个 store 用例 + 3 个 CLI 端到端用例。

- [x] 3.3 去重语义分离：幂等键管命令重放，来源身份管对象重复；同键异载荷返回冲突。
      **结果**：`018_delivery_idempotency.sql` 两张表（`delivery_command_receipts` 回执、
      `delivery_tracking_sources` 来源关系）。键的作用域是 `(company_id, key)`、**不含命令**——
      一个键换命令也是冲突；载荷指纹由内容派生（sha256 over 规范 JSON），同键异载荷拒绝；
      回执只写一次（trigger），且与业务写入同事务（`UnitOfWork::import_delivery_source`
      自带 `SAVEPOINT`，失败不污染键）。对象去重：五元组 + 来源关系，重复导入返回
      **已有跟踪关系**（`created_tracking=false`），不建第二个跟踪项；一个来源身份只承载
      一条跟踪关系，改挂明确拒绝。8 个 store 用例（含「两人两键导入同一 PR → 一个跟踪项」
      与「失败不污染键」）+ 10 个 `delivery_idempotency` 单测 + 2 个来源模块单测。
      **未做**：`POST /api/v1/delivery-sources/imports` 仍 `planned`，写入入口（含审计
      `append_audit_event` 与 §34.12 主体判定）随 3.5 落地——本段只有库层入口与用例。
- [x] 3.4 目标关联与 `expected_version` 追加语义；共享来源不合并不同客户记录。
      **结果**：`019_delivery_links.sql` 一张表（`delivery_tracking_work_packages`），
      目标关联与版本推进复用 016 的 `delivery_targets` 与 `resource_version`——目标身份索引
      本身就是去重键，再加一张关联表会让同一件事有两个来源。
      `delivery_link` 模块：引用恰好一个（`from_options`）、目标归一（`None` 与 `Some("")`
      都是整个环境，身份不 trim）、派生 `link_id`/`target_id`（长度前缀 + sha256，重试落同一行）。
      `UnitOfWork::append_delivery_link`：校验 → 读回执 → 期望版本闸门（不一致**什么都不写**）
      → 挂关系 → 补目标 → **有实际新增才 +1** → 写回执，整段 `SAVEPOINT`。
      纯重复 `appended=false` 且版本不动（否则双方互相判过期，重试变必冲突）；重放返回首次
      执行后的版本，拿它继续追加会 409 而非覆盖；已关闭的跟踪项不再接受关联（与
      `allowed_actions` 同口径）。
      **不合并不同客户记录**落在目标层：一个来源 + 两个客户 = 一条跟踪项 + 两个目标，
      A 的部署不进 B 的证据（`shared_source_keeps_each_customer_record_separate`）。
      11 个 `delivery_link` 单测 + 7 个 store 用例 + 1 个 CLI 端到端用例。
      **未做**：`POST /api/v1/delivery-tracking/{id}/links` 仍 `planned`（§34.12 未实现），
      所以仍无写入口；工作包关联只在 `bod tracking --id` 的文本里可见，不进 2.2 冻结的
      响应体；新追加的目标没有验收条件（请求体没有该字段），而 `acceptance` 目前也不是
      判定的输入——已记进 design 的 Open Questions。
- [ ] 3.5 关联与补证入现有 Company 单写事务；连接器与人工证据冲突保留两者并进待核实。

## 4. 共用覆盖投影（`I-72`）

- [ ] 4.1 把两条入口的原始事实规范化为同一组证据输入，交给 `coverage_receipt::judge_coverage`。
- [ ] 4.2 跨入口同结论用例：同一组证据 → 同一 `CoverageVerdict` 与同一证据等级（I-72 由 planned 起）。
- [ ] 4.3 只读接入权限边界：只读凭据可登记来源、不可部署、不可通知；对内/对外字段投影分离。

## 5. 草稿与发送（`delivery-draft`）

- [ ] 5.1 草稿装配（目标、收件范围、证据修订、引用、缺口、文本修订）与「保存不外发」。
- [ ] 5.2 无模型路径：规则摘要 + 人工编辑，结论与有模型时一致。
- [ ] 5.3 双点重验：命令受理点与出站点各验授权/证据修订/目标/账号；变化则进待重审并提示差异。
- [ ] 5.4 发送结果未知的对账；渠道无幂等能力时标待核实，不重发、不记失败。

## 6. 观测入站与纠正

- [ ] 6.1 部署/回滚/人工补证按来源序列处理乱序，更新受影响目标（`POST /delivery-observations`）。
- [ ] 6.2 回滚追加纠正记录并关联原答复；保留原答复与历史确认。
- [ ] 6.3 §11 验收表的 7 个场景各一条端到端用例，逐条对应 spec 的 Scenario。
