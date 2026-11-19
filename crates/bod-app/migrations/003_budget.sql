-- P0 D3 预算、预留与不可变账本迁移（任务 4.1—4.5）
-- 权威来源：V2.9.6 §11.3、§11.7、§11.10、§33.2（预算组：reservations、ledger_entries、usage_reports）。
-- 只创建表、索引与不可变触发器，不含 DROP/DELETE 等破坏性 DDL。

CREATE TABLE IF NOT EXISTS budget_limits (
    company_id TEXT NOT NULL,
    scope_kind TEXT NOT NULL
        CHECK (scope_kind IN ('company', 'project', 'agent', 'run', 'work_package')),
    scope_id TEXT NOT NULL,
    dimension TEXT NOT NULL
        CHECK (dimension IN ('money_micros', 'tokens', 'calls', 'duration_ms', 'disk_bytes', 'concurrency')),
    -- 非金额维度用空串；金额维度必须是三位币种，应用层校验（SQLite 无正则内建约束）。
    currency TEXT NOT NULL DEFAULT '',
    limit_value INTEGER NOT NULL,
    version INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (company_id, scope_kind, scope_id, dimension, currency),
    CHECK (limit_value >= 0 AND version >= 1),
    CHECK (dimension = 'money_micros' OR currency = '')
);

CREATE TABLE IF NOT EXISTS reservations (
    reservation_id TEXT PRIMARY KEY,
    company_id TEXT NOT NULL,
    project_id TEXT,
    agent_id TEXT,
    run_id TEXT,
    work_package_id TEXT,
    dimension TEXT NOT NULL
        CHECK (dimension IN ('money_micros', 'tokens', 'calls', 'duration_ms', 'disk_bytes', 'concurrency')),
    currency TEXT NOT NULL DEFAULT '',
    reserved_value INTEGER NOT NULL DEFAULT 0,
    confirmed_value INTEGER NOT NULL DEFAULT 0,
    risk_value INTEGER NOT NULL DEFAULT 0,
    state TEXT NOT NULL DEFAULT 'active'
        CHECK (state IN ('active', 'consumed', 'cancelled', 'risk')),
    logical_key TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    -- C/R/U 互斥：同一未结算金额只能处于 R 或 U；状态与量必须自洽（I-71 的 DB 防线）。
    CHECK (reserved_value >= 0 AND confirmed_value >= 0 AND risk_value >= 0),
    CHECK (NOT (reserved_value > 0 AND risk_value > 0)),
    CHECK (
        (state = 'active' AND risk_value = 0)
        OR (state = 'risk' AND reserved_value = 0)
        OR (state = 'consumed' AND reserved_value = 0 AND risk_value = 0)
        OR (state = 'cancelled' AND reserved_value = 0 AND risk_value = 0 AND confirmed_value = 0)
    )
);
CREATE INDEX IF NOT EXISTS idx_reservations_scope
    ON reservations(company_id, project_id, agent_id, run_id, work_package_id, state);
CREATE UNIQUE INDEX IF NOT EXISTS idx_reservations_logical_key
    ON reservations(logical_key) WHERE logical_key IS NOT NULL;

CREATE TABLE IF NOT EXISTS ledger_entries (
    entry_id TEXT PRIMARY KEY,
    company_id TEXT NOT NULL,
    project_id TEXT,
    agent_id TEXT,
    work_package_id TEXT,
    reservation_id TEXT,
    usage_key TEXT,
    entry_kind TEXT NOT NULL
        CHECK (entry_kind IN ('reserve', 'release', 'commit', 'risk_transfer', 'adjustment')),
    dimension TEXT NOT NULL
        CHECK (dimension IN ('money_micros', 'tokens', 'calls', 'duration_ms', 'disk_bytes', 'concurrency')),
    currency TEXT NOT NULL DEFAULT '',
    amount INTEGER NOT NULL,
    source TEXT NOT NULL,
    reason TEXT,
    created_at_ms INTEGER NOT NULL,
    CHECK (dimension = 'money_micros' OR currency = '')
);
-- 外部 usage_key 唯一：同一实际用量只入账一次（I-35）。
CREATE UNIQUE INDEX IF NOT EXISTS idx_ledger_usage_key
    ON ledger_entries(usage_key) WHERE usage_key IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_ledger_scope
    ON ledger_entries(company_id, project_id, agent_id, work_package_id);

-- 不可变账本：只追加，禁止改写或删除已入账分录（§11.3）。
CREATE TRIGGER IF NOT EXISTS ledger_entries_no_update
BEFORE UPDATE ON ledger_entries
BEGIN
    SELECT RAISE(ABORT, 'ledger entries are immutable');
END;

CREATE TRIGGER IF NOT EXISTS ledger_entries_no_delete
BEFORE DELETE ON ledger_entries
BEGIN
    SELECT RAISE(ABORT, 'ledger entries are immutable');
END;

-- 外部计量报告：usage_key 为唯一键，保留 provider/模型口径，便于不同维度分列。
CREATE TABLE IF NOT EXISTS usage_reports (
    usage_key TEXT PRIMARY KEY,
    company_id TEXT NOT NULL,
    provider TEXT NOT NULL,
    model TEXT,
    dimension TEXT NOT NULL
        CHECK (dimension IN ('money_micros', 'tokens', 'calls', 'duration_ms', 'disk_bytes', 'concurrency')),
    currency TEXT NOT NULL DEFAULT '',
    quantity INTEGER NOT NULL,
    final_report INTEGER NOT NULL DEFAULT 0,
    reported_at_ms INTEGER NOT NULL,
    CHECK (quantity >= 0),
    CHECK (dimension = 'money_micros' OR currency = '')
);
