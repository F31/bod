-- P8 §11 来源身份（3.2）：五元组、候选与追加式来源修订。
-- 权威来源：V2.9.8 §11「来源身份由五元组确定」；设计见 design.md D2。
--
-- 四条刻意的取舍：
-- 1. 身份是五元组（Company + Project + 连接实例 + 对象类型 + 提供方稳定对象 ID），
--    不是 URL。跨项目同名 PR 因此不会被合并：project_id 在唯一索引里。
-- 2. 只有定位（URL / 纯文本）时进**候选**，由负责人确认后才成为身份。
--    候选表与身份表分开：候选不是「待完善的身份」，它没有稳定 ID，也就没有身份。
-- 3. 来源修订**追加式**：同一标签先后解析到两个摘要要留两条（§11「可变标签」），
--    UPDATE/DELETE 由触发器拒绝。同一事件重复上报靠内容指纹幂等，不新增行。
-- 4. 观察与来源修订的连接用**关联表**（`delivery_observation_sources`），不给 016 的观察表加列：
--    `open` 每次都会重放全部迁移脚本，所有脚本必须天然幂等，而 SQLite 的
--    `ALTER TABLE ADD COLUMN` 没有 `IF NOT EXISTS`。关联表还顺手保证了
--    「一条观察最多一条来源修订」。连接器观察必须带来源修订这一条属导入路径（3.5），
--    在写入原语里强制，不在库里用 CHECK 一刀切。

CREATE TABLE IF NOT EXISTS delivery_sources (
    source_id TEXT PRIMARY KEY,
    company_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    -- 连接实例（GitHub App 安装、GitLab 项目连接等）。空串表示没有连接器实例：
    -- 人工补证与内部关联用空串，它表示「无」，不表示「未知」。
    connection_id TEXT NOT NULL DEFAULT '',
    -- 来源对象类型：`pull-request` / `commit` / `release` / `issue` 等。
    -- 不做白名单：连接器种类会持续增加，白名单会让新增连接器必须先改代码。
    object_type TEXT NOT NULL CHECK (length(trim(object_type)) > 0),
    -- 提供方稳定对象 ID（节点 ID、MR id、commit sha）。它才是身份的一半。
    provider_object_id TEXT NOT NULL CHECK (length(trim(provider_object_id)) > 0),
    -- 人类可读定位（URL 或 owner/repo#318）。只用于展示与候选，不参与身份判定。
    locator TEXT NOT NULL DEFAULT '',
    -- 谁确认这个身份（候选转正或直接登记时的操作者）。确认人不是「创建人」，
    -- 它是「这个 ID 确实是那个对象」的责任人。
    confirmed_by TEXT NOT NULL,
    confirmed_at_ms INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL
);

-- 身份唯一性：五元组。空串 connection_id 参与比较（SQLite 里 NULL 互不相等，
-- 用 NULL 会让「无连接器」的来源重复插入成功）。
CREATE UNIQUE INDEX IF NOT EXISTS idx_delivery_sources_identity
    ON delivery_sources(company_id, project_id, connection_id, object_type, provider_object_id);
-- 复合外键的父键：修订必须挂在同一公司的同一来源上。
CREATE UNIQUE INDEX IF NOT EXISTS idx_delivery_sources_tenant
    ON delivery_sources(company_id, source_id);
CREATE INDEX IF NOT EXISTS idx_delivery_sources_scope
    ON delivery_sources(company_id, project_id, created_at_ms);

CREATE TABLE IF NOT EXISTS delivery_source_candidates (
    candidate_id TEXT PRIMARY KEY,
    company_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    connection_id TEXT NOT NULL DEFAULT '',
    -- 对象类型可能尚未确定（只有一段 URL）：空串表示未知，不猜。
    object_type TEXT NOT NULL DEFAULT '',
    -- 定位：URL 或纯文本。候选必须有定位，否则无从确认。
    locator TEXT NOT NULL CHECK (length(trim(locator)) > 0),
    proposed_by TEXT NOT NULL,
    proposed_at_ms INTEGER NOT NULL,
    state TEXT NOT NULL DEFAULT 'pending'
        CHECK (state IN ('pending', 'confirmed', 'rejected')),
    -- 确认后指向的身份。pending / rejected 时为空。
    resolved_source_id TEXT,
    decided_by TEXT NOT NULL DEFAULT '',
    decided_at_ms INTEGER,
    -- 确认必须有身份与裁决时间；这两个条件写进库，而不是只靠应用层。
    CHECK (state <> 'confirmed'
           OR (resolved_source_id IS NOT NULL AND decided_at_ms IS NOT NULL
               AND length(trim(decided_by)) > 0)),
    -- 驳回必须记裁决时间与裁决人：拒绝一个候选也是决定。
    CHECK (state <> 'rejected'
           OR (decided_at_ms IS NOT NULL AND length(trim(decided_by)) > 0)),
    FOREIGN KEY (company_id, resolved_source_id)
        REFERENCES delivery_sources(company_id, source_id)
);

-- 同一作用域下同一个定位只留一条待确认候选：重复提交同一 URL 不该产生第二条待办。
CREATE UNIQUE INDEX IF NOT EXISTS idx_delivery_source_candidates_pending
    ON delivery_source_candidates(company_id, project_id, connection_id, locator)
    WHERE state = 'pending';
CREATE INDEX IF NOT EXISTS idx_delivery_source_candidates_state
    ON delivery_source_candidates(company_id, project_id, state, proposed_at_ms);

CREATE TABLE IF NOT EXISTS delivery_source_revisions (
    source_revision_id TEXT PRIMARY KEY,
    company_id TEXT NOT NULL,
    source_id TEXT NOT NULL,
    -- 提供方版本（commit sha、release tag、issue 修订）。空串表示版本未知。
    version TEXT NOT NULL DEFAULT '',
    -- 内容摘要（制品摘要 / 文件哈希）。空串表示当次观察未带摘要。
    digest TEXT NOT NULL DEFAULT '',
    -- 同一事件的幂等依据：由 version 与 digest 派生，**不含观察时间**。
    -- 含时间的话，重复上报就会变成两条「新修订」，去重也就不存在了。
    content_fingerprint TEXT NOT NULL CHECK (length(content_fingerprint) > 0),
    actor_id TEXT NOT NULL,
    -- 原始观察时间（来源时间），与入库时间分开存。
    observed_at TEXT NOT NULL,
    observed_at_ms INTEGER NOT NULL,
    recorded_at_ms INTEGER NOT NULL,
    FOREIGN KEY (company_id, source_id)
        REFERENCES delivery_sources(company_id, source_id)
);

-- 同一事件只确认一次：重复上报命中同一指纹 → 返回已有修订，不新增行。
CREATE UNIQUE INDEX IF NOT EXISTS idx_delivery_source_revisions_event
    ON delivery_source_revisions(company_id, source_id, content_fingerprint);
-- 可变标签的时间线：同一来源的多条修订按观察时间排，后一次不覆盖前一次。
CREATE INDEX IF NOT EXISTS idx_delivery_source_revisions_timeline
    ON delivery_source_revisions(company_id, source_id, observed_at_ms);

-- 修订是历史：标签重新解析出不同摘要要追加一条，不能把上一次观察改写成新的。
CREATE TRIGGER IF NOT EXISTS delivery_source_revisions_no_update
BEFORE UPDATE ON delivery_source_revisions
BEGIN
    SELECT RAISE(ABORT, 'delivery source revision is append-only');
END;

CREATE TRIGGER IF NOT EXISTS delivery_source_revisions_no_delete
BEFORE DELETE ON delivery_source_revisions
BEGIN
    SELECT RAISE(ABORT, 'delivery source revision is append-only');
END;

-- 候选只有从 pending 出发的一次状态转换；确认或驳回之后，任何改写都拒绝。
-- 「已确认的候选再改指向」会让身份与候选的关系失去唯一依据。
CREATE TRIGGER IF NOT EXISTS delivery_source_candidates_no_reopen
BEFORE UPDATE ON delivery_source_candidates
WHEN OLD.state <> 'pending'
BEGIN
    SELECT RAISE(ABORT, 'delivery source candidate is final once decided');
END;

-- 已确认的候选承载着「谁把定位认成了身份」这一事实，不允许删除。
CREATE TRIGGER IF NOT EXISTS delivery_source_candidates_no_delete_confirmed
BEFORE DELETE ON delivery_source_candidates
WHEN OLD.state = 'confirmed'
BEGIN
    SELECT RAISE(ABORT, 'confirmed delivery source candidate cannot be deleted');
END;

-- 观察挂来源修订：**用关联表而不是给 016 的观察表加列**。
-- 理由不是洁癖：`SqliteStore::open` 每次打开都会重放全部迁移脚本，所有脚本必须天然幂等。
-- `CREATE TABLE IF NOT EXISTS` 幂等，`ALTER TABLE ADD COLUMN` 不幂等（SQLite 没有
-- `IF NOT EXISTS`），第二次打开就会报 duplicate column。加列还得靠改迁移机制，
-- 而迁移机制一改，15 个既有脚本的「重放安全」假设也要重新论证——不划算。
--
-- 关联表还顺手保证了「一条观察最多一条来源修订」：observation_id 是主键。
CREATE TABLE IF NOT EXISTS delivery_observation_sources (
    observation_id TEXT PRIMARY KEY,
    company_id TEXT NOT NULL,
    source_revision_id TEXT NOT NULL,
    linked_at_ms INTEGER NOT NULL,
    FOREIGN KEY (observation_id) REFERENCES delivery_observations(observation_id),
    FOREIGN KEY (source_revision_id) REFERENCES delivery_source_revisions(source_revision_id)
);
CREATE INDEX IF NOT EXISTS idx_delivery_observation_sources_revision
    ON delivery_observation_sources(company_id, source_revision_id);
