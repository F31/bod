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

## 7. v1.0 承接：本 Change 即「切片 1 交付基线」

来源：`docs/智能体协作与可验证交付规范-v1.0.md`（T01–T07、C11–C13）与
`docs/bod智能体协作与可验证交付技术架构规格-v1.0.md` §9「切片 1　交付基线」。
v1.0 技术规格 §8 明确要求**复用** `authorization::access_record` 与
`coverage_receipt::judge_coverage`，因此不新建第二套判定。

**为什么不另开 change**：本 Change 的 `specs/delivery-draft` 已经写下了 T04/T06 的语义
（草稿不外发、无模型同结论、双点重验）。再建一个 change 重复这些需求，会得到两份会漂移的规格。
切片 1 的缺口在这里补齐，切片 2 起另立 `p9-adaptive-collaboration`。

### 7.1 条款映射

| v1.0 条款 | 本 Change 任务 | 现状 |
|---|---|---|
| T01 逐目标事实 | 3.1 / 3.6 | 读路径已 served，写入路径未开 |
| T02 来源与入口等价 | 2.1 / 4.1–4.2 | 判定有，imports 路由 planned |
| T03 目标级访问 | 3.6 | 判定与 1 条路由已落 |
| T04 两处外发核验 | 5.3 | 未开始 |
| T05 定向更正 | 6.2 + 7.2 | 只有观测追加，无 correction 对象 |
| T06 起草与验收 | 5.1 / 5.2 | 未开始 |
| T07 持续观察 | 7.3 | 未开始 |
| C11 目标级访问用例 | 3.6 | 已落（1 条端到端） |
| C12 两处分别失效 | 5.3 + 7.4 | 未开始 |
| C13 换执行者同结论 | 7.4 | 未开始 |

### 7.2 更正对象（T05 / C05）

> **需求先行**：`openspec/specs/` 里目前**没有** `delivery-tracking` 与 `delivery-draft`
> 两个能力（p8 尚未归档），本 Change 的 `specs/*/spec.md` 是它们唯一的来源。
> 7.2 的语义现在只写在下面这几行任务里、没有写进 spec delta——那么归档时主线规格里
> 就永远不会出现「更正对象」，下一个人会按主线规格重新发明一套。所以 7.2.0 先于 7.2.1。

- [x] 7.2.0 先写 `specs/delivery-draft/spec.md` 的 ADDED requirement：
      「更正以独立对象追加，不覆盖原答复与原确认」，场景至少覆盖
      （a）`coverage=complete` 时已知下游逐个有状态；
      （b）无法穷举下游时 `coverage=partial` 且**不得**出现「全部送达」的表述；
      （c）通知失败留可重试记录。`openspec validate --strict` 通过后 7.2.1 才开工。
      **已落（2026-11-23）**：新增 requirement「更正以独立对象追加，不覆盖原答复」
      （三场景齐全，且写明覆盖范围由系统判定、不得由调用方声明）；顺带把原来的
      「回滚追加纠正而不覆盖」改名为「目标事实与各级确认分别记录」——它真正的主张是
      「回执≠送达≠确认，不得互相推断」，与对象形状是两件事，不该挤在一条里。
- [ ] 7.2.1 `correction` 对象：`previous`（被更正记录引用）、`operation`（withdraw / supersede）、
      `reason`、`affected`、`coverage`（complete / partial）、`replacement`（supersede 必填）。
      追加式，禁止改写原答复与原确认。
- [ ] 7.2.2 更正可送达已知下游；无法穷举下游时 `coverage=partial`，**不得宣称全部送达**。
      通知失败保留可重试记录。
- [ ] 7.2.3 用例：唯一来源撤回且旧答复已外发 → 原记录可查、当前结论待重评、已知下游有更正状态；
      历史决定页保留原时点签署。

### 7.3 观察委任（T07）

- [x] 7.3.0 先写 `specs/delivery-tracking/spec.md` 的 ADDED requirement：
      「主动持续观察须有独立委任」，场景覆盖（a）无委任不得启动；
      （b）委任到期 → 停止新增但保留历史；（c）观测中断/过期/撤权 → 对外显示「陈旧」或「未知」，
      不回显上一次结论（这一条与已有结论的展示语义冲突，必须在规格里写清楚哪个优先）。
      没有这条 delta，7.3.1 的实现就是归档后查不到的能力。
      **已落（2026-11-23）**：新增 requirement「主动持续观察须有独立委任」，三场景齐全；
      并写明委任与执行指派必须分开记录（撤指派不等于撤委任），以及「陈旧/未知」优先于
      「上一次结论」——后者不写清，实现十有八九会拿旧结论填。
- [ ] 7.3.1 主动持续观察须有**独立委任**：目标、期限、频率、预算、允许动作、停止条件。
      缺委任不得启动观察。
- [ ] 7.3.2 观测中断/过期/撤权时对外显示「陈旧」或「未知」，不显示上一次结论。
- [ ] 7.3.3 用例：委任到期 → 停止新增观察且保留历史；撤权 → 即时阻断读取与外发。

### 7.4 C12 / C13 用例

- [ ] 7.4.0 先确认 `specs/delivery-draft/spec.md` 里 C12/C13 的两条语义是否已可判定：
      现有 requirement 只写了「发送必须在两个生效点各重验一次」与「回滚追加纠正」，
      没有写「两处**分别**失效时的不同后果」，也没有写「换执行者/加模型起草不改变结论」。
      缺则先补 delta，再写用例——避免「用例通过但需求里没有这条」。
      **已落（2026-11-23）**：C12 的「两处后果不同」写进已有 requirement 正文
      （受理前失效不留待发意图；入队后失效转待重审、不得静默丢弃）并补一条场景；
      C13 单列为 requirement「执行者与起草方式的替换不改变结论」。
- [ ] 7.4.1 C12：在受理前与入队后外调前**分别**注入证据失效；前者拒绝且无待发意图，
      后者阻断并转待重审。两条用例独立，不得合并成一条。
- [ ] 7.4.2 C13：更换执行者或加入模型起草，目标证据、策略与授权不变 → 覆盖结论逐字相同；
      起草与验收分别记录，草稿保存无外发。

### 7.5 路由收口顺序

- [x] 7.5.1 前置：**主体装配入口**（见 7.6）。在此之前任何路由转 served 都只是「测试里能读」。
      **已满足（commit `079d679`）**：`serve --db` 之后 GET 由真实请求读通（200），
      未装配仍 401。下面的 7.5.2 因此可以逐条推进。
- [x] 7.5.2 **原计划（五条 POST 逐条转 served）已被 ADR-0009 作废**（2026-11-23 裁定选 B）。
      保留原文在这里不是为了留痕好看，而是为了记住它为什么错：它假设「接口是写入入口」，
      而本 Change 的非目标明写「不建第二业务写入者」。两条同时成立是不可能的。
      按 B，五条 `POST` **保持 `planned`**，`x-bod-status` 不改——这不是未完成，是决定。
      只有在 ADR-0009 改判为 A（并同时落下宿主锁与 epoch）时，才恢复「逐条转 served」。
- [ ] 7.5.3 B 之下的等价物：为每条写入给出 **CLI 子命令**，顺序不变
      （imports → links → observations → drafts → send），每条都要有：
      子命令 + 真实库上的端到端用例 + 「谁调用它」的一句话（哪类角色、在什么场合跑）。
      一条没做完之前不开下一条。拆成五条子项，是因为这条总项「做完」之前
      看起来会一直像没动——而它实际上是一条一条长出来的。
- [x] 7.5.3.1 `bod tracking import`（第一条）。**已落（2026-11-23）**：
      - 子命令：`bod tracking import --db <path> --key <幂等键> --project <项目>
        --locator <定位> [--connection / --object-type / --provider-object-id]
        [--title / --requirement / --owner] [--correlation / --decided-at] [--json]`。
      - **谁在什么时候跑它**：本机操作者（项目成员）收到一个外部交付信号之后——
        客户报了一个缺陷、某个 PR 合进了客户分支——把「这件事发生在哪」变成可跟踪的来源。
        它是 ADR-0009 选 B 之后交付写入的第一条真实入口：HTTP 五条 `POST` 仍 `planned`，
        `serve` 仍只读挂载。
      - 三个 `new_*_id` 由来源身份派生（`ImportSourceCommand::with_derived_ids`）：
        它们进载荷指纹，随机生成会让「同键重试」变成「同键异载荷」。派生所用的
        「长度前缀摘要」从 `delivery_link` 上移到 `canonical_json::length_prefixed_digest`，
        两份实现不再各写一遍。
      - `imported_by` 取本机账户（`local_principal_id`），`--as` **明确拒绝**而不是
        「不认识」；取不到主体就在碰库之前失败，不建库。
      - 写库走事务：`store.transaction()` → `uow.import_delivery_source` → `commit`。
        回执、业务写入与审计同生共死（3.5 的 SAVEPOINT 在里层），外层不提交等于什么都没发生。
      - CLI 面第一次非空：`CLI_ENTRIES = [(link-source, "bod tracking import")]`，
        `bod tracking --id` 改为真实装配事实（`cli_allowed_actions`），
        哨兵用例转正向断言（见 7.8.5）。
      - 证据：`delivery_tracking_cli` 15 passed（+5：导入→重放→换键不重复建项、
        同键异载荷拒绝且不影响首次导入、`--as` 拒绝且不建库、
        允许动作只给成员、入口表与 `--help` 对照）；`delivery_source` 新增一条派生 ID 用例。
      - **已知缺口（写在这里而不是藏起来）**：写入侧**不判项目成员资格**。
        `allowed_actions` 会按成员资格决定要不要推荐 `link-source`，但 `import` 不拦——
        因为 `project_members` 至今没有生产写入者，加闸门会让所有人都导不进来（包括该导的人）。
        等 `project_members` 有入口（B3）再收紧。**不得**因此说「写入口已按权限放行」。
- [x] 7.5.3.2 `bod tracking link`（原 links）：追加工作包关联，复用
      `UnitOfWork::append_delivery_link` 与 `expected_version` 语义；谁调用它＝
      操作者在「这条交付对应哪个工作包/PR」确定之后补挂。**已落（2026-11-23）**：
      - 子命令：`bod tracking link --db <path> --id <跟踪项> --key <幂等键>
        --expected-version <n> (--source <来源 ID> | --work-package <工作包修订>)
        [--target <客户>:<环境>[:<部署单元>]]… [--correlation / --decided-at] [--json]`。
      - **谁在什么时候跑它**：操作者在「这条交付对应我们哪一段工作」确定之后补挂。
        导入回答「这件事发生在哪」，`link` 回答「它对应哪个工作包 / 哪个外部 PR」——
        两者分开是因为它们常常不是同时知道的：缺陷先到，归属后定。
      - **`--expected-version` 必填，命令自己不读当前版本**。这是本轮唯一的判断点。
        HTTP 上版本由请求体携带；命令行上若让命令自己「读一次再写」，读与写之间那段
        窗口里别人的写入会被静默吞进本次决定——闸门 100% 不会触发，等于把 3.4 的期望版本
        删掉。因此流程是两步：先 `bod tracking --id <跟踪项> --json` 读 `version`，再带上来写。
        代价是交互式使用时多一步；**收益是「过期」这个答案真的存在**——它有测试
        （`link_refuses_a_stale_expected_version_and_names_the_current_one`），
        而且冲突时错误里直接给当前版本与可照抄的下一步命令，避免被当成 bug 报上来。
      - 二选一（`--source` / `--work-package`）交给 `LinkRef::from_options`：它已经区分
        「都没给」与「都给了」，CLI 不重写一遍判断——多一处规则就多一处会漂移的地方。
      - `--target` 按 `<客户>:<环境>[:<部署单元>]` 解析，可重复；**已有目标按身份四段命中**
        （不是按 `target_id`），缺的才新建。新建目标没有验收条件与开关状态，
        所以补挂一个未验收目标会把结论从「全覆盖」退回「有缺口」——那不是副作用，是事实。
      - **`CLI_ENTRIES` 语义纠正**：`link-source` 现在指向 `bod tracking link`。
        7.5.3.1 为了让 CLI 面非空，曾把它登记到 `bod tracking import`；那是权宜——
        视图说「你能 link」而命令在 import，两边指的不是同一件事。
        `import` 是创建跟踪项的入口（可能在任何跟踪项都还没有时跑），不在
        「对这条**已有**跟踪项能做什么」的值域里，所以它不占动作码。
      - 已知缺口与 7.5.3.1 同一条：写入侧**不判项目成员资格**（`project_members` 仍无
        生产写入者）。`allowed_actions` 会在读取时按成员资格决定是否推荐 `link-source`，
        但 `link` 不拦。**不得**因此说「写入口已按权限放行」。
      - 证据：`delivery_tracking_cli` 21 passed（+6：追加工作包并推进版本、
        过期版本拒绝且指出当前版本与下一步、同键重放返回首次结果里的版本而非今天的、
        引用「都不给」与「都给」分别报错、缺期望版本时说明为何不代读、
        按身份补目标只建缺的那个）。
- [x] 7.5.3.3 `bod tracking observe`（原 observations）：登记部署/回滚/人工补证；
      乱序按来源序列处理，人工补证的空备注由写入原语拒绝。**已落（2026-11-23）**：
      - 子命令：`bod tracking observe --db <path> --id <跟踪项> --key <幂等键>
        --kind deployment|rollback|manual-evidence --target <客户>:<环境>[:<部署单元>]
        --observed-at <RFC3339> [--source-kind connector|manual|internal]
        [--source-ref / --source-revision / --artifact-revision / --digest /
        --revision / --note] [--correlation / --decided-at] [--json]`。
      - **谁在什么时候跑它**：操作者或集成方在「一次部署、一次回滚、一份离线安装回执」
        发生之后登记。它同时是**补录**的唯一入口——几十小时前发生的事今天才录。
      - 四个判断，每条都有对应用例：
        1. **`--observed-at` 必填，命令不默认「现在」**。观察的业务时间是**来源时间**，
           不是入库时间。默认「现在」会让晚到的补录把一周前的部署记成今天，
           于是「回滚是否晚于部署」与时间线排序一起失效，**而且不报错**。
           代价是每条命令多一个参数；收益是「昨天部署、今天才录」仍然是昨天。
        2. **不推进版本，也不要 `--expected-version`**。`link` 改的是关系集合，
           有可被覆盖的读-改-写窗口，所以需要期望版本；观察只往时间轴追加——
           两个人同时各记一条部署，两条都该留下。用例
           `recording_an_observation_does_not_bump_the_version` 与
           `two_writers_record_without_conflict` 各钉一半（后者的意义是：若版本参与，
           第二条会以「照着旧状态做决定」为由被拒，而两件事实之间没有决定关系）。
        3. **`authorized` 不由命令行给出，恒写 0**。授权范围是 §34.12 的判定结果；
           提供 `--authorized` 等于让被审者自己打勾，与 ADR-0007「主体不得自报」同一种错。
           未授权的补证不会让覆盖变成「已覆盖」（判定侧给 `install-receipt-unauthorized`），
           这是安全的一侧，不是漏了。
        4. **观察不创建目标**。`link` 的 `--target` 是「**要确保存在**的目标」，
           观察的 `--target` 是「这条事实**发生在**哪个目标上」。凭一条可能打错客户名的
           观察凭空建出客户记录，会把一次笔误变成一份对外承诺。目标不存在就报错，
           错误里直接给可照抄的 `link` 命令。
      - **乱序**：判定取最新按 `observed_at`（业务时间），不按写入顺序。用例
        `a_late_arriving_older_rollback_does_not_flip_the_conclusion`：先记今天的部署，
        再补录昨天的回滚，结论仍是「已部署」。可执行的含义是——补录能补上证据，
        不能改写先后。
      - **观察 ID 派生而不是生成**（`obs-` + 长度前缀摘要）。进 ID 的是「发生了什么」
        （公司/跟踪项/目标/类别/来源类别/业务时间/修订/摘要/制品修订/来源修订/备注），
        不进的是「怎么记的」（谁记的、人类可读坐标、幂等键）。同一事实重复上报落回
        同一行（`recorded=false`，且**不记审计**——重复上报不该在审计里多出一次事实）。
        `source_kind` 必须进 ID：连接器与人工对同一时刻可以各有一条，
        合并成一条，冲突就消失了。
      - **时间自己解析**（本仓不引时间库）：接受 RFC 3339 常用子集，规范化到 UTC 毫秒。
        闰秒、`+0800`（无冒号）、`2026-02-30` 一律拒绝——它们不是「宽一点」，是错值；
        小数秒截到毫秒是存储精度，不是猜测。同一时刻只允许一种写法，
        否则观察 ID 会因写法分叉，重发就变成两条。
      - **三条拒绝各给可照抄的下一步**：未挂目标 → 给 `link` 命令（带上操作者刚给的目标）；
        人工补证空备注 → 指出要写授权范围与复验依据；声称连接器却无来源修订 →
        同时给 `--source-revision` 与 `--source-kind manual` 两条正当出路，
        否则只剩「换个词蒙过去」。
      - **顺带修一处通道缺失**：CLI 写命令的错误此前只打自述、**从不带稳定码**
        （各变体都有 `code()`，但文本通道没用过），而 HTTP 一侧码就是契约。
        新增 `StoreError::as_code()` + `write_failure`，三个写命令统一输出
        `自述（码）` + 下一步，并去掉「追加关联失败：关联追加失败:」这种叠两层的旧前缀。
        故障类（IO/SQL/pragma）**不给码**——给了会诱使调用方按码分支，
        把「重试可能有用」与「改请求才有用」混成一件事（见 7.10.10）。
      - 证据：`delivery_tracking_cli` **29 passed**（21 → 29，+8）；
        `store::tests::observation` **10 passed**（含乱序一条）；
        `delivery_observation` 模块 6 条（时间归一与拒绝、ID 不进记录者、
        连接器须带来源修订、缺省来源类别、载荷按毫秒比、落库恒未授权）。
      - **已知缺口**：（a）与 7.5.3.1 同一条——写入侧**不判项目成员资格**
        （`project_members` 仍无生产写入者），**不得**说「写入口已按权限放行」；
        （b）观察**不进** `allowed_actions`：动作面的值域是「对这条**已有**跟踪项能做什么」，
        当前登记过的只有 `link-source` 与 `draft`，没有观察的路由；
        为一个没有契约的动作造一个新码，是自己发明契约。
      - 交互约束：`--target` **恰好一个**。一条事实落在多个客户目标上时分条登记——
        允许一条命令写多个目标，会让一条打错的目标被同一条事实顺带覆盖，
        而判定是按目标做的。
- [ ] 7.5.3.4 `bod tracking draft`（原 drafts）：装配草稿，**不外发**；无模型路径同结论。
- [ ] 7.5.3.5 `bod tracking send`（原 send）：两处重验 + 发送结果未知的对账。
      这一条的 `action_requirement` 不得沿用 `Membership`——它有对外后果。

### 7.6 主体模型（唯一硬分叉点）

- [x] 7.6.1 写 ADR：依据 v1.0 技术规格 §1「互不信任租户按 OS 身份或部署边界隔离」
      与 §0.4「先确定目标和对象可见性，再判动作授权」，主体由**宿主装配时按本地身份确定**，
      请求体/请求头**不得自报**主体。否决 `X-Bod-Actor` 方案：自报主体会让租户隔离失效，
      并使审计归属可被调用方改写。
      **已落（commit `86607c2`，2026-11-22 晚）**：`adr/0007-subject-and-target-authorization.md`
      第 6 条 + 三条撤销条件；落地记录见同文件末尾（含「为什么只读挂载不违反 8.1」的判据）。
- [x] 7.6.2 落地：`HostServer` 的真实启动路径调用 `with_delivery(read, actor)`；
      完成判据——未带任何自报头的真实请求返回 200 而不是 401，且换一个 id 仍按目标过滤。
      **已落（2026-11-22 深夜）**，三处落点与五条证据：
      1. `host_handshake::local_principal_id`：启动时读一次本机账户名（`USERNAME`/`USER`/`LOGNAME`），
         逐字作为主体；**不提供 `--as` 覆盖**（覆盖开关等于把主体交给启动者）。
      2. `SqliteStore::open_existing_readonly`：`SQLITE_OPEN_READ_ONLY` + `query_only`，
         不建库、不迁移、不写——只读副本不是「第二个写者」，不违反 8.1。
      3. `bod serve --db <path> [--company <id>]`：显式路径不存在即启动失败；缺省只在
         `$BOD_HOME/bod.db` 已存在时挂载；未挂载时 `/api/v1/*` 一律 401，不回退成匿名主体。
      证据（`crates/bod/tests/delivery_read_e2e.rs`，真实子进程 + 真实 TCP）：无自报头 → 200；
      换 id → 404；带 `X-Bod-Actor: someone-else` → 仍 404；未挂载 → 401 `BOD_API_DENIED`
      且不建库；显式缺库 → 启动失败且不留空库/描述文件。
- [x] 7.6.3 已知限制写进支持矩阵：同一台机器多个成员共用时，审计归属会记到宿主身份上。
      该限制不得用「多用户支持」的说法掩盖。
      **已落**：`openspec/changes/p8-delivery-tracking/design.md` §「支持范围与已知限制（7.6.3）」
      第 2 行（主体来源）与第 4 行（交付域只有读路径、事实表无写入者）。

### 7.7 写者归属（7.5 的硬前置，ADR-0009）

背景：7.6.2 的合法性靠「只读副本不是写者」；7.5 一旦转 served，控制面就必须持可写连接，
成为本 Change 非目标里明确排除的「第二业务写入者」（`proposal.md` §5）。这不是排期问题，
是自相矛盾，必须先裁定。

- [x] 7.7.1 签 `adr/0009-writer-ownership.md`：**2026-11-23 裁定选 B**（写入只走 CLI）。
      A 保留为 p9/p10 的目标形态，切换触发条件是「出现非本机进程必须写入的真实需求」。
- [x] 7.7.2 若裁为 A 的那一支：**不适用**（裁定为 B）。留作 A 之下的硬约束备案——
      届时锁与写路由必须同一提交，中间态就是无防护双开。
- [x] 7.7.3 三个「只读」命令改为只读挂载（commit `7ae60cf`）：`observe` / `tracking` / `sources`
      此前都走 `SqliteStore::open`，每次打开跑 20 个迁移、缺路径还 `create_dir_all` 建库。
      现改为 `open_existing_readonly`，缺库报 `MissingDatabase`。用例
      `read_commands_refuse_missing_db_instead_of_creating_it` 对三条命令各跑一次真实二进制。
- [ ] 7.7.4 遗留登记（已写进支持矩阵第 1 行）：即便选 B，**两个 CLI 进程之间仍无锁**。
      WAL 把写串行化、不损坏数据，但「宿主独占写锁 + epoch」仍未实现，
      所以任何报告都不得声称「已防双开」。单实例锁要独立设计（锁文件位置、僵死检测、
      重启接管、与 `$BOD_HOME` 的关系），不在本 Change 内顺手做。

### 7.8 动作可用性判定（7.5 的前置，与 7.7 的选项无关）

- [x] 7.8.1 记录现状：`allowed_actions` 由 `is_served_action` 过滤，只认「路由是否 served」；
      两个动作码（`link-source` / `draft`）的路由都是 `planned`，所以该字段**恒为空**。
- [x] 7.8.2 判定改为四道关：候选 → 该调用面有入口（`available_on(ActionSurface)`：
      HTTP 看 `served`、CLI 看 `CLI_ENTRIES`）→ 需求已登记（`action_requirement`，
      未登记即 fail-closed）→ 当事人满足需求（`authorization::requirement_met`，目前只实现
      `Membership`，其余需求一律 false）。`CLI_ENTRIES` 当时是**空表**，用哨兵用例钉住；
      7.5.3.1 落地后转正向断言（见 7.8.5）。
- [x] 7.8.3 投影不再自己判：`TrackingView::to_dto(allowed_actions)` 只渲染，
      判定在 `delivery_api::select_allowed` + `authorization`。`evaluate_read` 把
      facts 与 actor 一起传下去（此前手里有 actor 却不往下传）。CLI 的 `--json` 当时传空
      并注明原因（CLI 面无入口），不假装「这个主体什么都不能做」；
      7.5.3.1 之后 CLI 改为真实装配（`cli_allowed_actions`）。
- [x] 7.8.4 反例记录：`readonly_api`（912 行，仅测试可达）有 `ReadonlyAction` /
      `authorize_readonly` / `button_hint`，看着像动作授权，实际只判项目级可达，注释明写
      「界面提示只用于显示，不参与放行判定」。**不得**当作现成实现复用；
      与 `evidence_publication` ≠ V05 属同一类命名坑。
- [x] 7.8.5 用例（`delivery_api::action_rules`，原六条，现七条）：
      （a）没入口 → 不列（即使当事人确实有权限）；
      （b）有入口但需求未登记 → 不列（合成的 `teleport` 码）；
      （c）有入口 + 是成员 → 列；非成员 → 不列；缺项目 → 不列；
      （d）`CLI_ENTRIES` **原为哨兵**（变非空即红），7.5.3.1 落地后转正向断言
      `every_cli_entry_is_wired_to_a_registered_requirement`（每条目的要么有登记需求、
      要么有子命令全文、要么出现在 `available_on(Cli)`，缺一即红）+
      `the_cli_surface_advertises_link_source_to_a_member`（面非空时动作真的出现）；
      7.5.3.2 起表里那条指向 `bod tracking link`——真正承载 `link-source` 的子命令；
      `bod tracking import` 不再占码（它创建跟踪项，不在「对已有跟踪项能做什么」的
      值域里，理由见 7.5.3.2）；
      （e）`available_on(Http)` **仍是哨兵**：两条动作路由仍是 planned，变非空即要求
      补断言与端到端。
      另有 `bod` crate 的 `the_command_named_in_the_entry_table_actually_exists`：
      拿 `--help` 对照入口表，挡住「表里写着、命令不存在」。


### 7.9 防复发门禁

- [x] 7.9.1 `reach --check` 进 CI（linux job，紧跟迁移检查）。**已落（2026-11-23）**。
      同时刷新了基线 header：原来写 `lines=107484`，实际已 109,005——差 1,521 行而
      `--check` 照样绿。刷新前后逐行比对，`live 72 / orphan 14 / test_only 8` 一行未变，
      只有行数与排序变化。
      **补记（`70eb8e7`）**：进 CI 不等于在 CI 上跑过。它第一次真正执行是 `70eb8e7`，
      此前一直被「全量测试」的红挡在前面（step 12 永远 skipped）。那次结论 success。
      换句话说，这条门禁从 2026-11-23 起才算真的存在。
- 7.9.2 **口径（不是待办，是约束）**：`reach --check` 是弱门禁——只比模块归属，不比行数。
      它挡得住「新孤岛」，挡不住「既有模块膨胀」。因此不得用它证明「不可达比例没有上升」；
      那个比例要单独读数，并写清命令（本文件以外的报告也一样）。

### 7.10 持续集成长期红灯（2026-11-23 发现）

- [x] 7.10.1 **让红灯可诊断**（commit `be9a843`、`18c08f4`）。
      linux job 的「全量测试」改成 `... | tee $RUNNER_TEMP/cargo-test.log`，失败时
      `upload-artifact`；再把失败用例名与失败段原文写成 **check-run 注解**——
      日志与 artifact 都要登录，注解是公开可读的。
      **第一次改进自己坏了**：诊断步骤被 `-e` + `pipefail` 干掉（`grep` 没匹配到行退出 1），
      step 9 反而红，注解一个字都没写出去。已改 `set +e` 且失败段兜底取 `tail`。
      教训：诊断代码也要按「它自己不能失败」写。
- [x] 7.10.2 拿到第一个失败用例名（`be9a843` 的注解）：
      `impact_sample::tests::real_commit_sample_is_selective_and_keeps_unknown`
      ——`xtask` 用例，跑 `HEAD~1..HEAD` 的真实提交。
      **根因**：`actions/checkout` 默认 `fetch-depth: 1`，浅克隆没有父提交。
      本地复现：`git clone --depth 1` 后 `git rev-parse HEAD~1` →
      `fatal: ambiguous argument 'HEAD~1'`。
      这解释了为什么**连纯文档提交也红**：红的是环境假设，不是代码。
      修复（commit `99e315d`）：两个 rust job 都加 `fetch-depth: 0`；
      用例的失败信息改写为点名「浅克隆需要 fetch-depth: 0」，别让下一个人去翻判定逻辑。
- [x] 7.10.3 第二个成因（读 `a2eee1d` 的注解拿到）：
      `p4_exit::every_cited_target_exists_on_disk` ——「报告引用的 spec note
      `p4-verification` 不存在」；`p5_exit` 同款。
      **根因**：`988b1b0`（chore: 文档与规格手册改为仅本地保留）把 `docs/` 移出仓库
      （`.gitignore:54`，`git ls-files docs` = 0），而这两个用例断言
      `docs/spec-notes/*.md` 存在。也就是说这个门禁的对象本身**不在仓库里**：
      CI 按定义拿不到，于是天天红——红的不是代码，是要求 CI 验证一件它没有的东西。
      **改法（不是放宽门禁）**：note 检查变成「有 docs 树才是真检查」——`docs` 在则
      逐条断言（本机照旧会红，含 `docs` 在但 `spec-notes` 缺的情况）；`docs` 缺席则
      打一行 skip 到 stderr，**不假装验过**。仓库内守得住的两组（测试目标、模块）
      仍是**无条件硬断言**，任务勾选与重复编号的检查也没动。
      **两条分支都真跑过（不是推理）**：
      ① 本机有 docs：`cargo test -p bod-app --test p4_exit --test p5_exit` → 3/3 + 3/3；
      ② 无 docs：`git archive HEAD` 造一棵只有 tracked 文件的树（里面没有 `docs/`），
      再把 `CARGO_MANIFEST_DIR` 指向同样没有 docs 的布局跑这两份**同样的源码** →
      3/3 + 3/3，且 stdout 打出
      `skip: ...\docs 不存在，spec note 的存在性本次未校验（docs/ 只在本机保留）`。
- [x] 7.10.4 注解取用例名的正则修了一处：上一版要求至少一个 `::`，于是长在测试二进制
      根上的名字（`every_cited_target_exists_on_disk`）被整条丢掉，注解写着
      「没认出用例名」——**比不给名字更误导**，会让人以为失败不在断言上。
      改成 `awk` 只在 `failures:` 之后的缩进清单里取、允许单段名；本地用一份含两种
      形态的假日志验过。windows job 也补了同一套 tee + artifact + 注解：两个平台都红过，
      只诊断一边等于把另一半留在黑暗里。
- [x] 7.10.5 判据（这次真正的收获）：**在同一个工作目录里跑，永远复现不了「仓库缺文件」
      这类红灯**。本机和 WSL 都挂 `/e/projects/bod`，`docs/` 都在，所以两边全量都是
      1450/0——红灯只有在「只有 tracked 文件」的树里才现形。
      复现命令：`git archive HEAD | tar -x -C <dir>`，在那棵树里跑测试。
      另外，`docs/` 连重命名都不行（目录删除同样被拦），别指望用「临时藏起来」验证。
- [x] 7.10.6 **结论（`b9aafe8`）**：`rust (windows)` 整条绿；`rust (linux)` 的
      「全量测试」也绿了——**红了五天的测试步骤到此为止**，两个成因（浅克隆、
      `docs/` 不在仓库里）都确认修掉。windows 侧从没绿过，这是第一次。
      唯一还红的是第 11 步「迁移检查」——它过去一直被前一步的红挡住，**这是它
      第一次真正在 CI 上跑**，见 7.10.7。
- [x] 7.10.7 迁移检查一跑就红：`[migration-check] FAIL 未登记的迁移脚本:
      015_audit.sql, 016_delivery_tracking.sql, 017_delivery_sources.sql,
      018_delivery_idempotency.sql, 019_delivery_links.sql, 020_stage_facts.sql`
      ——P8 期间加的六个迁移从没登记进 `xtask` 的 `MIGRATION_TABLES`，
      而那个表的作用正是「新增迁移必须被检查覆盖，否则失败而不是静默放过」。
      本机复现：`cargo run -q -p xtask -- migration-check`。
      补登记（`inspect_migration_tables` 逻辑一字未改，只是把这六个文件真正纳入
      「必需表在不在、有没有破坏性 DDL」的范围）→
      `PASS 20 个迁移脚本含 64 张必需表且无破坏性 DDL`。
      另记一条洞（**没修，只写下来**）：检查只验「登记的表在不在」，
      **不验「文件里有没有没登记的表」**，所以多建表不会被拦。
- [x] 7.10.8 **CI 全绿（`70eb8e7`）**——四个 job 全 success，且是第一次跑完整条
      linux 流水线：`全量测试` → `门禁 G-1/G-50` → `迁移检查` → `不可达基线`
      → `规格索引` 五步全部 success（前四步里有三步此前从未在 CI 上执行过）。
      读数来源：`GET /repos/F31/bod/actions/runs` 过滤 `70eb8e7`，再取
      `/actions/runs/{id}/jobs` 的 step 级 conclusion。
      **这之后才可以说「CI 绿」**——而它只覆盖到这一刻的 `70eb8e7`，不是永久状态。
- [x] 7.10.9 **又一次新红（`2e57e54`）**：`rust (windows)` 的「全量测试」失败，唯一用例
      `handoff_sample::tests::two_real_runtimes_reach_an_evidenced_l2_claim`——
      第二个运行时（node `v22.23.5`）`fail@produce/produce_no_output`，用时 15152 ms，
      正好压着 `SAMPLE_TIMEOUT`（15 s）。与 7.5.3.2 的改动无关（`handoff_sample` 未触碰）。
      本机不可复现：本机 node `v22.22.2` / Python `3.13.14` 全绿，CI 是 node `v22.23.5` /
      Python `3.12.10`。**不猜**，只做两件不依赖复现的事：
      （a）**让现场可读**：`SampleConfig` 加默认方法 `diagnostic_tail()`，
      `DurableSampleConfig` 覆写为「会话目录 `events.log` 尾部原文」（每会话 5 行、
      总长截到 800 字符）。此前 `produce_no_output` 只说「读到 0 条观察」，分不清
      「进程崩了（stderr 有报错）」「活着但什么都没写（文件空）」「写到别处」——
      三者处置完全不同，而 CI 上不可复现，现场只能靠这段原文带出来。
      用例 `failed_sample_carries_the_session_log_tail` 钉住它（含「空目录返回 None，
      不编一段现场」）。
      （b）**抹平一处实现不一致**：Python 分支一直显式 `flush=True`，node 分支靠
      `console.log` 的运行时默认。样本按「行落进会话文件」判断有没有产出，刷新时机
      不该由运行时决定 → node 改成 `fs.writeSync(1, …)`。
      **没有**加超时余量、**没有**改成 skip：那两种都是让红灯消失，不是让原因消失。
      **结果（两次，同一份代码）**：`a42e00a` 四个 job 全 success、该用例通过；
      `515b201`（纯文档提交，`bod-app` 一字未改）同一个用例再红，而这次注解里
      **第一次带出现场**：`inv-sample-…-a/events.log: （空或不存在）`，用时 15058 ms。
      两件事因此确定：① 它是**环境抖动**，不是回归；② 不是「写了没刷新」——
      文件是空的，不是写了半截（那条 node 同步写改动因此**不是**修复，只让两个实现一致，
      保留）。抖动正好压在超时边界上：两次 15152 / 15058 ms，都是把 15 s 用满。
      本机热启动只需几百毫秒 → 15 s 是本地标定值，调到 60 s，依据写在
      `SAMPLE_TIMEOUT` 旁的注释里。**放宽窗口不动判定**：仍要真拿到两个检查点、
      失败照样报实测耗时与现场。判据：下一次 60 s 后仍空 → 不是慢，回去查 node 的
      `-e` 在这台 runner 上到底发生了什么；若绿且耗时 ~16 s → 「冷启动」成立。
      **`28a2aad` 结果**：四 job 全绿，该用例通过。但这次绿**同样不构成证明**——
      绿的那一轮不打印该用例耗时，所以分不清「60 s 窗口容下了冷启动」与「这轮没抖动」。
      判据仍挂着，只是红灯暂时没出现。
- [x] 7.10.10 **CLI 文本通道从不带稳定错误码**（7.5.3.3 暴露出的一处通道缺失）。
      HTTP 一侧每个「请求被拒」都有稳定码（`DELIVERY_*` / `IDEMPOTENCY_*` / `LINK_*` /
      `OBSERVATION_*`），而 CLI 写命令只打 `Display`（自述），码从未出现在 stderr 上——
      于是「照着码搜文档、按码写脚本」这条路在命令行上根本不存在，只是此前没人注意到，
      因为旧用例只断言人类可读文案。三条新用例（连接器缺来源修订、未挂目标、
      人工补证空备注）同时红，才把它逼出来。
      修法不是给每个命令各写一遍映射，而是 `StoreError::as_code()` + 一个 `write_failure`：
      码放自述之后（终端先看「什么错了」，再往下才是可搜的码），
      **故障类（IO/SQL/pragma）没有码**——给 `SQLITE_BUSY` 编一个可分支的码，
      会让「重试可能有用」和「改请求才有用」看起来像同一类答案。
- [x] 7.10.11 **又一次 windows 红（`138475e`）**：`host_guard_e2e` 的
      `dns_rebinding_style_host_is_rejected` 与 `duplicated_host_headers_are_rejected`
      在 `TcpStream::connect` 处失败，`Os { code: 10061 }`（连接被拒）；同一二进制里
      另外 8 条通过。本机 3/3 全绿（0.24 s），linux job 绿。
      **不知道根因，所以没有冒充知道**。能确定的是两件事，且都值得修：
      （a）**现场被丢掉了**：测试用 `Stdio::null()` 起子进程，于是「描述文件写出来了、
      但连不上」这个现象只剩一句 10061——分不清「进程在写出描述文件后退出」
      「端口被系统收回」「连接建立前被外部拦」。改成把子进程 stderr 落进会话目录，
      失败时一并带出 `child.try_wait()` 状态、描述文件内容与 stderr 尾部；
      等待描述文件时**先看进程有没有退出**，别把一个「启动失败」报成「启动慢」。
      诊断函数自己有用例 `diagnosis_survives_missing_artifacts`——救火时抛 panic
      的诊断等于没有诊断。
      （b）**描述文件会说谎**：它由 `HostServer::bind_with_feed` 在 bind 之后立刻写出，
      而 `serve` 之后还有能失败的步骤（装配产品接口、打印就绪行）。任何一步失败，
      进程退出但文件留在盘上，消费方看到的就是「地址在服务」而连接被拒。
      加 `DescriptorCleanup`（RAII）：`serve` 进程退出时一定撤掉描述文件，
      正常停止与启动中途失败都走这条路径。
      **判据**：下一次若再红，注解里会带出子进程 stderr；到时候按原文判，
      不按现在的猜测判。若从此转绿，只能说明「这一轮没复现」，不是根因已修。
      **结果（`8e5b75f`）**：四个 job 全 success，windows 的「全量测试」success，
      `host_guard_e2e` 通过。按上面的判据，这**不构成根因已修的证据**——两处修的都是
      确定性缺陷（现场丢失、描述文件说谎），而不是被观测到的那个 10061 本身。

