# Tasks: P8 双入口交付

> 规格来源：本 Change 的 `specs/*/spec.md`（V2.9.8 §11；V2.9.9 增补 §34.15 拒绝码分级与
> §29.8「两处重验必须能分别失败」，见 `docs/bod-V2.9.9-产品方案勘误.md`）。全部条目**未开始**——
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
      **已落地（部分）**：冲突判定在 `delivery_tracking::digest_conflict`——按 `revision` 分组，
      同一版本出现两个摘要才判冲突，结论停在 `PendingVerification` 并写明是哪一组版本；
      两条观察原样留在时间线里，不折叠、不择优。判据必须带版本：只比摘要会把每次正常发版
      判成冲突。3 个命名用例（冲突停在待核实且两份证据都在 / 新版本换摘要不算冲突 /
      两来源同摘要不算冲突）。
      **已落地（审计接线）**：`import_delivery_source` 与 `append_delivery_link` 在各自的
      `SAVEPOINT` 内写一条 `AuditEvent`——审计与业务写入同事务，审计写不进去整条回滚
      （`import_rolls_back_when_the_audit_row_is_refused` 证明跟踪项、来源、回执、审计四张表
      一起归零）。为此把审计的动作词表从 `StageAction` 上摘下来成 `AuditAction`（ADR-0007
      第 1 条「不扩 `StageAction`」的延伸）：准入动作走 `from_stage`，业务写入走 `for_write`，
      互相喂错类型即拒。`correlation_id` 由命令携带而不是拿幂等键顶替——键名含 `secret`
      就会被脱敏闸门判成秘密，让整条导入永久失败（这个坑是被用例当场撞出来的）。
      只有真新增才记审计：`appended=false` 的幂等空操作不写。5 个 store 用例 +
      5 个 `audit_log` 单测。
      **未做**：写入口（`POST /api/v1/delivery-tracking/imports` 等仍 `planned`）、
      §34.12 主体判定、**拒绝路径的审计**（拒绝会回滚事务，连带带走审计行；
      要留痕必须另起事务，这是独立的一步）。
      另缺一条用例：「观察写入与既有 Company 写操作同事务」的跨域回滚
      （现在各自都走 `UnitOfWork`，但没有一条用例证明 issue 写入失败时观察一并回滚）。

- [ ] 3.6 `GET /api/v1/delivery-tracking/{id}` served + 主体/目标判定接线（ADR-0007 `proposed`）。
      **为什么排在写端点之前**：§39.25 的判定类场景都要读回覆盖结论；写入口先上、过滤后补，
      等于把「没有过滤」发布成「已过滤」。判定函数已在 `authorization`
      （`filter_records` / `access_record`），缺口是 `StageFacts` 的只读装配端口与
      `Principal` 的传参，不是重写判定。
      **盘点更正（V2.9.9 规格审阅）**：`StageFacts` 六组事实里只有 `grants` 能直接查。
      `members`(001) **没有 `project_id`**；`customer_bindings` / `release_operators` /
      `assignments` / `revoked_at_revision` **无表或无从派生**；`Principal` 在**生产代码零构造**，
      而 `Auth::Bearer` 只证明「同一用户在本机」、不携带身份。所以本项的真实顺序是
      **补 schema → 定主体来源 → 装配 → 路由**，直接上 GET 会装配出空事实，
      让每个请求都 `BOD_NOT_FOUND` 且与「对象不存在」不可区分。分解见
      `docs/reports/v299-spec-review-and-plan.md` §2 轨 B（B1 迁移 / B2 装配端口 /
      B3 `Principal` 传参 / B4 装配失败不降级 / B5 转 served / B6 `assignments` 语义）。
      B1 的三张表：`project_members(company, project, principal, revision, active)`、
      `customer_bindings(company, principal, customer)`、
      `release_operators(company, principal)`；撤权修订另找来源（`config_revisions` 或专用表）。
      `members`(001) 保持 Company 级身份，不塞 `project_id`——Project 是授权边界，
      Company 才是数据边界（V2.9.9 勘误 §5.11）。
      出口条件：三档拒绝码（`BOD_API_DENIED` / `BOD_NOT_FOUND` / `BOD_SCOPE_DENIED`）各有命名用例；
      客户协作者读到兄弟目标返回 `BOD_NOT_FOUND` 而非 403；`x-bod-status` 由 `planned` 改 `served`。
      **轨 A 已具备的前置（判定层，未接线）**：三档码由 `authorization::RecordRejection` 给出并各有命名用例
      （`rejection_codes_are_three_and_the_subject_stage_comes_first`：主体先于对象，未认证时答案与对象无关）；
      `record_visible` 已拆为 `is_visible`（阶段二）与 `target_visible`（阶段四），可分别归因
      （`visibility_and_target_granularity_fail_for_different_reasons`）；
      `delivery_api::DENIED_CODE` 改由 `RecordRejection::ApiDenied` 派生，同一档码不再有两个字面量。
      仍缺 B1/B2/B3：路由上没有任何一处调用这些函数，`x-bod-status` 仍是 `planned`。
      **B1/B2/B6 已完成（2026-11-22）**：
      - B1 → `020_stage_facts.sql` 四张表：`project_members`（PK company+project+principal）、
        `customer_bindings`（PK company+principal，**一个主体只绑定一个客户**：一对多会让
        `customer_of` 的答案取决于行序）、`release_operators`（PK company+principal）、
        `authorization_revocations`（PK company+revision，`principal_id` 为空表示公司级，
        追加式：UPDATE/DELETE 由 trigger 拒绝）。四张表都带 `revision` 与 `active`：
        它们是**当前事实**，历史由 `audit_events` 承担。
      - B6 → `assignments` **不建表**，从既有 `issues.owner_id / owner_kind /
        assignment_grant_revision` 派生。那三列是写路径唯一维护的指派事实，再建一张表
        就是第二份会漂移的真相（且没有写入者）。
      - B2 → `store::facts::StageFactsPort`（`crates/bod-app/src/store/facts.rs`）：
        只读、按主体投影、失败返回 `StageFactsError`（`BOD_FACTS_UNAVAILABLE` / 503），
        绝不返回空事实。10 条用例覆盖空库、跨公司/跨主体隔离、停用行、表外权限文本报错、
        缺表报错、客户绑定唯一性、撤权表追加式、以及「装配 → `access_record` → 三档码」的接缝
        （`assembled_facts_drive_the_record_decision`）。
      - **撤权修订的来源已定**：只认 `authorization_revocations`。**不从 `grants.active = 0`
        反推**——`grants.revision` 是授予时的修订，停用不 bump，推出来偏旧，
        会让本该重同步的离线客户端漏一次重同步。
      **B4/B5 已完成（2026-11-22）**：
      - **B4 → 装配失败对外映射**：`StageFactsError::CODE`（`BOD_FACTS_UNAVAILABLE`）
        + `http_status()`（503）由 `evaluate_read` 直接使用。用例
        `unreadable_facts_are_503_and_never_an_empty_snapshot` 同时守**顺序**：
        假实现里 `tracking_view` 直接 panic，证明「装配失败后不得继续读视图」。
      - **B5 → `GET /api/v1/delivery-tracking/{id}` 转 `served`**，四阶段判定接线：
        主体 → 容器立场 → 目标粒度；三档拒绝码全部经 `RecordRejection` 派生；
        `x-bod-status` 已改 `served`，501 响应已从契约移除。
      - **新增判定出口** `authorization::ContainerRead` / `read_container`（三档：
        `Whole` / `ByTarget` / `Halted`）。读请求**不发 `StageAction`**——ADR-0007 第 1 条：
        往里塞一个 `Read` 会逼出「读取需要 Dispatch 凭据吗」这类没有答案的问题。
        容器对客户协作者是「按目标放行」，不是「过 / 不过」：容器本身不带客户维度，
        用布尔只能得到「恒不可见」或「对所有客户敞开」，两个都错。
      - **新增只读端口** `delivery_api::TrackingRead`（`Send + Sync`）+
        `DeliveryActor`（公司 + 主体，主体本身不带公司，这是部署事实）。
        库侧实现 `store::read_port::StoreTrackingRead`：`Mutex<SqliteStore>`，
        因为服务循环本身是单线程串行（`HostServer::run` 只有 SSE 才 spawn），
        锁上没有争用——它只是把「`Connection` 不是 `Sync`」这件事说出来。
      - **装配入口唯一**：`HostServer::with_delivery(read, actor)`，**两个依赖合成一个
        `Option`**：有数据源没主体、或有主体没数据源，都是「装配没完成」，
        拆成两个字段会让半配置在类型上合法。未装配时一律 401 `BOD_API_DENIED`，
        且对任何 id 同答（用例 `a_host_without_an_actor_denies_before_looking_at_the_id`
        两个 id 一起断言）。

      **仍未做 / 读之前必看**：
      - **B3 主体来源仍未经产品决定**（A 单用户显式 / B 会话 → 成员映射 /
        C `X-Bod-Actor` 请求头自报）。接缝已就位（主体由宿主传给 `with_delivery`，
        接口层不猜、不硬编码），但**没有任何生产入口调用 `with_delivery`**，
        所以当前真实请求仍回 401。契约标 `served` 的含义是「端点已实现且行为可解释」，
        **不是「线上已经能读」**——这两件事在报告里必须分开写。
      - `authorization_revocations` **已有写入者**（2026-11-22，见下条）；剩下的
        「谁有权下撤权命令、命令入口在哪」仍属 B3 同一件事。
      - 目标级**证据收窄是近似**：`EvidenceStamp` 不带 `target_id`（形状 2.2 已冻结），
        只能保留「被保留结论所依据的那几版修订」。要精确到目标得改契约形状。
      - `allowed_actions` 仍只按「端点是否 served」过滤，**未按动作权限判定**。
        当前所有写端点 planned，所以它恒为空；接通任何写端点之前必须补上，
        否则视图会推荐一个主体实际做不了的动作。

      **撤权写入者已完成（2026-11-22，`store::revoke`）**：
      - `UnitOfWork::revoke_authorization(RevokeCommand, now_ms)`：四张事实表按
        (company, principal) 置 `active = 0`、追加 `authorization_revocations` 一行、
        写审计 `revoke-authorization`，三件事同一个 SAVEPOINT。审计写不进去整条回滚
        （用例用「相关性 ID 含 `secret` 形态」触发真实失败，断言 grants 仍生效、无撤权行）。
      - **修订号在事务内现算**（`MAX(revision) + 1`），不由调用方给：填错会让离线客户端
        少重同步一次（`check_subscription` 的 `since < revoked_at`）。
      - **已是目标状态就 no-op**：判据是「有没有生效授权」，不是「有没有撤权记录」
        ——撤权后又被重新授权时记录还在，必须能再撤一次并 bump 修订。
      - 只做**主体级**撤权（`principal_id` 非空）。公司级（`principal_id` 为空，
        影响全公司）读侧 SQL 已能处理，但**没有写入者**：产品侧不存在「一次抹掉整公司
        授权」的命令，凭空实现一个危险操作比缺它更糟。
      - **已知缺口，用例钉住**（`assignment_alone_still_permits_work_after_revocation`）：
        `StageRequirement::Work` 只判 `target.assignee`，不查成员资格也不查 Grant，
        所以**撤权后被指派者仍能「开发与进度回报」**。这与「撤权不改写历史分配」一致，
        但「保留归属」不等于「保留执行权」。要收紧得让 `Work` 同时要求成员资格，
        属判定语义变更，需产品决定。
      - 顺带修正一处**系统性误伤**：审计 `basis` 的前缀是 `<action>:`（程序常量），
        原先整段过脱敏闸门，于是 `revoke-authorization:` 撞上 `authorization:` 标记，
        撤权审计永远写不进去且报错只说「basis 含凭据」。现在只剥掉「本事件动作名 +
        冒号」再检查正文，剥不掉就整段检查（防冒充前缀），三条用例分别守前缀、冒充、正文。

## 4. 共用覆盖投影（`I-72`）

- [ ] 4.1 把两条入口的原始事实规范化为同一组证据输入，交给 `coverage_receipt::judge_coverage`。
- [ ] 4.2 跨入口同结论用例：同一组证据 → 同一 `CoverageVerdict` 与同一证据等级（I-72 由 planned 起）。
- [ ] 4.3 只读接入权限边界：只读凭据可登记来源、不可部署、不可通知；对内/对外字段投影分离。

## 5. 草稿与发送（`delivery-draft`）

- [ ] 5.1 草稿装配（目标、收件范围、证据修订、引用、缺口、文本修订）与「保存不外发」。
- [ ] 5.2 无模型路径：规则摘要 + 人工编辑，结论与有模型时一致。
      判定侧的钉子已提前落地（`verdict_does_not_depend_on_any_drafter`）：`CoverageInput` 只有六组事实，
      用例用穷尽解构做编译期哨兵——将来给判定加任何草稿/摘要字段都会先编译失败，逼着先回答
      「它是不是判定输入」。草稿装配本身仍未做。
- [ ] 5.3 双点重验：命令受理点与出站点各验授权/证据修订/目标/账号；变化则进待重审并提示差异。
- [ ] 5.4 发送结果未知的对账；渠道无幂等能力时标待核实，不重发、不记失败。

## 6. 观测入站与纠正

- [ ] 6.1 部署/回滚/人工补证按来源序列处理乱序，更新受影响目标（`POST /delivery-observations`）。
- [ ] 6.2 回滚追加纠正记录并关联原答复；保留原答复与历史确认。
- [ ] 6.3 §11 验收表的 7 个场景各一条端到端用例，逐条对应 spec 的 Scenario。
