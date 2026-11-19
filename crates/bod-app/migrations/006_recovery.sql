-- P0 D6 迁移登记、备份核验、对账与清理（任务 7.1—7.4）
-- 权威来源：V2.9.6 §33.7 备份/迁移/恢复、§8.5 生命周期与删除、§33.9 对账与清理表。
-- 只创建表与索引，不含破坏性 DDL。

CREATE TABLE IF NOT EXISTS migration_registry (
    migration_id TEXT PRIMARY KEY,
    checksum TEXT NOT NULL,
    -- 逗号分隔的 migration_id；为空表示无依赖。
    depends_on TEXT NOT NULL DEFAULT '',
    schema_range TEXT NOT NULL DEFAULT '',
    software_range TEXT NOT NULL DEFAULT '',
    risk TEXT NOT NULL DEFAULT 'safe' CHECK (risk IN ('safe', 'destructive')),
    compatibility_window TEXT NOT NULL DEFAULT '',
    backup_notes TEXT NOT NULL DEFAULT '',
    data_notes TEXT NOT NULL DEFAULT '',
    applied_at_ms INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS backups (
    backup_id TEXT PRIMARY KEY,
    company_id TEXT NOT NULL,
    path TEXT NOT NULL,
    db_sha256 TEXT NOT NULL,
    db_bytes INTEGER NOT NULL DEFAULT 0,
    -- 缺少 WAL 的快照不得标为可恢复（§33.7）。
    wal_included INTEGER NOT NULL DEFAULT 0,
    -- 快照方法：vacuum_into / file_copy；用于解释 WAL 是否已被合并。
    snapshot_method TEXT NOT NULL DEFAULT '',
    cas_object_count INTEGER NOT NULL DEFAULT 0,
    config_revision TEXT NOT NULL DEFAULT '',
    adapter_versions TEXT NOT NULL DEFAULT '',
    key_recovery_note TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL CHECK (status IN ('verified', 'incomplete')),
    problems TEXT NOT NULL DEFAULT '',
    created_at_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_backups_company ON backups(company_id, created_at_ms);

CREATE TABLE IF NOT EXISTS reconciliation_cases (
    case_id TEXT PRIMARY KEY,
    company_id TEXT NOT NULL,
    invocation_id TEXT,
    logical_action_key TEXT NOT NULL,
    impact_scope TEXT NOT NULL DEFAULT '',
    -- 保留的风险占用引用：Unknown 不按普通 TTL 回收。
    risk_reservation_refs TEXT NOT NULL DEFAULT '',
    known_evidence TEXT NOT NULL DEFAULT '',
    assignee TEXT NOT NULL CHECK (length(trim(assignee)) > 0),
    next_step TEXT NOT NULL CHECK (length(trim(next_step)) > 0),
    state TEXT NOT NULL DEFAULT 'open' CHECK (state IN ('open', 'resolved', 'abandoned')),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_reconciliation_state
    ON reconciliation_cases(company_id, state, created_at_ms);

CREATE TABLE IF NOT EXISTS cleanup_requests (
    request_id TEXT PRIMARY KEY,
    company_id TEXT NOT NULL,
    target_kind TEXT NOT NULL,
    target_id TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'requested'
        CHECK (state IN ('requested', 'blocked', 'completed', 'abandoned')),
    -- 逐项阻塞原因：执行停止、费用结算、产物固定、临时环境。
    blockers TEXT NOT NULL DEFAULT '',
    requested_by TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    -- 同一目标重复请求幂等，不新建第二条清理记录。
    UNIQUE (company_id, target_kind, target_id)
);
CREATE INDEX IF NOT EXISTS idx_cleanup_state ON cleanup_requests(company_id, state);

CREATE TABLE IF NOT EXISTS cleanup_abandons (
    abandon_id TEXT PRIMARY KEY,
    company_id TEXT NOT NULL,
    request_id TEXT NOT NULL,
    residual_resources TEXT NOT NULL DEFAULT '',
    risks TEXT NOT NULL DEFAULT '',
    owner TEXT NOT NULL CHECK (length(trim(owner)) > 0),
    reconciliation_entry TEXT NOT NULL CHECK (length(trim(reconciliation_entry)) > 0),
    created_at_ms INTEGER NOT NULL
);
