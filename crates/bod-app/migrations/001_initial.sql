-- P0 D1 初始迁移（任务 2.6）
-- 权威来源：V2.9.6 §33.2 表与索引、§8.1 资源外壳、§37.5 SQLite 写线程策略。
-- 本脚本只创建表与索引，不含 DROP/DELETE 等破坏性 DDL；后续破坏性变更走独立迁移。

CREATE TABLE IF NOT EXISTS events (
    event_id TEXT PRIMARY KEY,
    company_id TEXT NOT NULL,
    aggregate_id TEXT NOT NULL,
    aggregate_version INTEGER NOT NULL,
    schema_version INTEGER NOT NULL DEFAULT 1,
    payload TEXT NOT NULL,
    UNIQUE (aggregate_id, aggregate_version)
);
CREATE INDEX IF NOT EXISTS idx_events_company ON events(company_id, event_id);

CREATE TABLE IF NOT EXISTS issues (
    uid TEXT PRIMARY KEY,
    company_id TEXT NOT NULL,
    project_id TEXT,
    kind TEXT NOT NULL,
    repo_provider TEXT,
    repo_id TEXT,
    title TEXT NOT NULL,
    acceptance TEXT NOT NULL DEFAULT '',
    owner_id TEXT,
    owner_kind TEXT,
    assignment_grant_revision INTEGER,
    resource_version INTEGER NOT NULL DEFAULT 1,
    generation INTEGER NOT NULL DEFAULT 1,
    observed_generation INTEGER NOT NULL DEFAULT 1,
    phase TEXT NOT NULL DEFAULT 'Pending',
    condition_reason TEXT,
    deletion_requested_at TEXT,
    CHECK (resource_version >= 1 AND generation >= 1)
);
CREATE INDEX IF NOT EXISTS idx_issues_scope ON issues(company_id, project_id);

CREATE TABLE IF NOT EXISTS work_package_revisions (
    revision_id TEXT PRIMARY KEY,
    company_id TEXT NOT NULL,
    issue_uid TEXT NOT NULL,
    spec_revision TEXT NOT NULL,
    acceptance_revision TEXT NOT NULL,
    created_at TEXT NOT NULL,
    UNIQUE (issue_uid, spec_revision)
);

CREATE TABLE IF NOT EXISTS runs (
    uid TEXT PRIMARY KEY,
    company_id TEXT NOT NULL,
    issue_uid TEXT NOT NULL,
    plan_revision TEXT NOT NULL,
    desired_state TEXT NOT NULL,
    resource_version INTEGER NOT NULL DEFAULT 1,
    generation INTEGER NOT NULL DEFAULT 1,
    observed_generation INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE IF NOT EXISTS attempts (
    uid TEXT PRIMARY KEY,
    company_id TEXT NOT NULL,
    run_uid TEXT NOT NULL,
    invocation_key TEXT NOT NULL UNIQUE,
    status TEXT NOT NULL,
    observed_at TEXT
);

CREATE TABLE IF NOT EXISTS leases (
    resource_key TEXT PRIMARY KEY,
    company_id TEXT NOT NULL,
    owner TEXT NOT NULL,
    lease_token INTEGER NOT NULL DEFAULT 0,
    lease_expires_at TEXT,
    state TEXT NOT NULL DEFAULT 'idle'
);

CREATE TABLE IF NOT EXISTS members (
    uid TEXT PRIMARY KEY,
    company_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    display_name TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS grants (
    principal_id TEXT NOT NULL,
    company_id TEXT NOT NULL,
    project_id TEXT,
    permission TEXT NOT NULL,
    revision INTEGER NOT NULL,
    active INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (principal_id, company_id, permission, revision)
);
CREATE INDEX IF NOT EXISTS idx_grants_scope ON grants(company_id, project_id, principal_id);

CREATE TABLE IF NOT EXISTS config_revisions (
    revision INTEGER PRIMARY KEY,
    body TEXT NOT NULL,
    created_at TEXT NOT NULL
);
