-- P8 §11 外部交付跟踪（3.1）：跟踪项、客户目标与追加式观察。
-- 权威来源：V2.9.8 §11；设计与盘点见 openspec/changes/p8-delivery-tracking/design.md（D1—D3）。
--
-- 只创建新表，不改既有列。四条刻意的取舍：
-- 1. 跟踪项上的内部引用（issue/run/attempt）**全部可空**：外部交付不得为了满足约束
--    创建占位运行（D3 的禁止路径）。
-- 2. 观察表**追加式**：回滚是新的观察，不是把部署观察改判为失败；UPDATE/DELETE 由触发器拒绝。
-- 3. 表间用复合外键约束「公司 + 跟踪项 + 目标」三者一致，避免观察挂到别人的目标上。
--    这些约束只涉及本迁移新建的表，不改变任何既有表的可写性，也不引入内部运行身份。
-- 4. 覆盖结论**不落库**：它由 `coverage_receipt::judge_coverage` 在读时算出（D1），
--    表里只放事实（观察、开关、人群、验收条件），避免出现第二份会漂移的结论。

CREATE TABLE IF NOT EXISTS delivery_tracking (
    tracking_id TEXT PRIMARY KEY,
    company_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    title TEXT NOT NULL CHECK (length(trim(title)) > 0),
    -- 需求/变更摘要。空串表示尚未提供，不用 NULL 表示「没填」，避免两种空值并存。
    requirement TEXT NOT NULL DEFAULT '',
    -- 负责人：空串表示未指派。没有负责人时不得声称「已安排人跟进」。
    owner_id TEXT NOT NULL DEFAULT '',
    state TEXT NOT NULL DEFAULT 'open' CHECK (state IN ('open', 'closed')),
    -- 内部引用：可空。外部完成的研发不要求内部运行存在（D3）。
    issue_uid TEXT,
    run_uid TEXT,
    attempt_id TEXT,
    resource_version INTEGER NOT NULL DEFAULT 1 CHECK (resource_version >= 1),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_delivery_tracking_scope
    ON delivery_tracking(company_id, project_id, created_at_ms);
-- 复合外键的父键：让子表能引用「同一公司的同一跟踪项」。
CREATE UNIQUE INDEX IF NOT EXISTS idx_delivery_tracking_tenant
    ON delivery_tracking(company_id, tracking_id);

CREATE TABLE IF NOT EXISTS delivery_targets (
    target_id TEXT PRIMARY KEY,
    company_id TEXT NOT NULL,
    tracking_id TEXT NOT NULL,
    customer_id TEXT NOT NULL,
    -- 目标环境标识（客户侧命名，不要求全局唯一）。
    environment TEXT NOT NULL,
    -- 部署单元/客户组。空串表示「整个环境」：SQLite 里 NULL 在唯一索引中互不相等，
    -- 用空串才能真的唯一标识一个目标。
    deployment_unit TEXT NOT NULL DEFAULT '',
    -- 验收条件（规则修订或人可读条款）。空串表示尚未提供，覆盖结论必然停在待核实。
    acceptance TEXT NOT NULL DEFAULT '',
    -- 功能开关与人群是**当前事实**，不是观察；观察只记录部署/回滚/人工补证。
    switch_state TEXT NOT NULL DEFAULT 'unknown'
        CHECK (switch_state IN ('synced', 'not-synced', 'unknown')),
    population_known INTEGER NOT NULL DEFAULT 0 CHECK (population_known IN (0, 1)),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    FOREIGN KEY (company_id, tracking_id)
        REFERENCES delivery_tracking(company_id, tracking_id)
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_delivery_targets_identity
    ON delivery_targets(company_id, tracking_id, customer_id, environment, deployment_unit);
-- 复合外键的父键：观察必须落在同一跟踪项下的目标上。
CREATE UNIQUE INDEX IF NOT EXISTS idx_delivery_targets_ref
    ON delivery_targets(company_id, tracking_id, target_id);

CREATE TABLE IF NOT EXISTS delivery_observations (
    observation_id TEXT PRIMARY KEY,
    company_id TEXT NOT NULL,
    tracking_id TEXT NOT NULL,
    target_id TEXT NOT NULL,
    -- deployment：连接器观察到的部署；rollback：回滚观察；manual-evidence：人工补证。
    kind TEXT NOT NULL CHECK (kind IN ('deployment', 'rollback', 'manual-evidence')),
    -- 来源**类别**只用于证据投影；来源**身份**（五元组）在 3.2 建表，
    -- 此处只保留人类可读定位（`source_ref`），它不参与任何判定。
    source_kind TEXT NOT NULL CHECK (source_kind IN ('connector', 'manual', 'internal')),
    source_ref TEXT NOT NULL DEFAULT '',
    -- 证据载荷
    artifact_revision TEXT,
    digest TEXT NOT NULL DEFAULT '',
    revision TEXT NOT NULL DEFAULT '',
    note TEXT NOT NULL DEFAULT '',
    actor_id TEXT NOT NULL,
    -- 不是请求字段：由 §34.12 的授权判定后写入。缺省 0 表示未授权，
    -- 未授权的人工补证因此不会让覆盖结论变成「已覆盖」。
    authorized INTEGER NOT NULL DEFAULT 0 CHECK (authorized IN (0, 1)),
    -- 业务观察时间（来源时间，不是入库时间）；乱序重排按它与 recorded_at_ms 判定。
    observed_at TEXT NOT NULL,
    observed_at_ms INTEGER NOT NULL,
    recorded_at_ms INTEGER NOT NULL,
    -- 人工补证必须写清授权范围与复验依据：空备注的补证不构成证据（§11）。
    CHECK (kind <> 'manual-evidence' OR length(trim(note)) > 0),
    FOREIGN KEY (company_id, tracking_id, target_id)
        REFERENCES delivery_targets(company_id, tracking_id, target_id)
);
CREATE INDEX IF NOT EXISTS idx_delivery_observations_timeline
    ON delivery_observations(company_id, tracking_id, observed_at_ms);
CREATE INDEX IF NOT EXISTS idx_delivery_observations_target
    ON delivery_observations(company_id, target_id, observed_at_ms);

-- 观察是历史：回滚追加新记录，不原地改写。删除同样拒绝——「删掉那条部署观察」
-- 等于让已经发出去的答复失去依据。
CREATE TRIGGER IF NOT EXISTS delivery_observations_no_update
BEFORE UPDATE ON delivery_observations
BEGIN
    SELECT RAISE(ABORT, 'delivery observation is append-only');
END;

CREATE TRIGGER IF NOT EXISTS delivery_observations_no_delete
BEFORE DELETE ON delivery_observations
BEGIN
    SELECT RAISE(ABORT, 'delivery observation is append-only');
END;
