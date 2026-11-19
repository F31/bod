-- P0 D5 成果、修订与接受（任务 6.1—6.4）
-- 权威来源：V2.9.6 §21.1 统一成果模型、§21.9 验收证据、§33.2 表组、§33.5 CAS 顺序。
-- 只创建表、索引与不可变约束；不含破坏性 DDL。

CREATE TABLE IF NOT EXISTS artifacts (
    uid TEXT PRIMARY KEY,
    company_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    kind TEXT NOT NULL,
    title TEXT NOT NULL,
    owner_id TEXT NOT NULL,
    visibility TEXT NOT NULL DEFAULT 'internal'
        CHECK (visibility IN ('internal', 'customer', 'audit')),
    -- 接受指针经 compare-and-swap 更新；与内容寻址 CAS 不是同一概念（§21.1）。
    accepted_revision TEXT,
    resource_version INTEGER NOT NULL DEFAULT 1 CHECK (resource_version >= 1),
    created_at_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_artifacts_scope ON artifacts(company_id, project_id, uid);

CREATE TABLE IF NOT EXISTS artifact_revisions (
    revision_id TEXT PRIMARY KEY,
    company_id TEXT NOT NULL,
    artifact_id TEXT NOT NULL,
    base_revision TEXT,
    storage_kind TEXT NOT NULL CHECK (storage_kind IN ('cas', 'external')),
    storage_ref TEXT NOT NULL,
    digest_algorithm TEXT NOT NULL DEFAULT 'sha256',
    digest TEXT NOT NULL,
    size_bytes INTEGER NOT NULL CHECK (size_bytes >= 0),
    mime TEXT NOT NULL,
    produced_by TEXT NOT NULL,
    source_note TEXT NOT NULL DEFAULT '',
    created_at_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_artifact_revisions_artifact
    ON artifact_revisions(artifact_id, created_at_ms);
CREATE INDEX IF NOT EXISTS idx_artifact_revisions_digest
    ON artifact_revisions(company_id, digest);

CREATE TABLE IF NOT EXISTS artifact_dependencies (
    revision_id TEXT NOT NULL,
    depends_on_revision TEXT NOT NULL,
    kind TEXT NOT NULL
        CHECK (kind IN ('implements', 'built_from', 'tests', 'documents', 'derived_from', 'supersedes')),
    PRIMARY KEY (revision_id, depends_on_revision, kind)
);

CREATE TABLE IF NOT EXISTS verification_records (
    record_id TEXT PRIMARY KEY,
    company_id TEXT NOT NULL,
    revision_id TEXT NOT NULL,
    state TEXT NOT NULL
        CHECK (state IN ('pending', 'verifying', 'verified', 'rejected', 'needs_revision', 'waived')),
    actor_id TEXT NOT NULL,
    reason TEXT NOT NULL DEFAULT '',
    evidence_refs TEXT NOT NULL DEFAULT '[]',
    verification_rules TEXT NOT NULL,
    environment TEXT NOT NULL,
    observed_at_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_verification_records_revision
    ON verification_records(revision_id, observed_at_ms);

CREATE TABLE IF NOT EXISTS waivers (
    waiver_id TEXT PRIMARY KEY,
    company_id TEXT NOT NULL,
    revision_id TEXT NOT NULL,
    actor_id TEXT NOT NULL,
    reason TEXT NOT NULL CHECK (length(trim(reason)) > 0),
    scope_company TEXT NOT NULL,
    scope_project TEXT,
    scope_issue TEXT,
    granted_at_ms INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS acceptances (
    acceptance_id TEXT PRIMARY KEY,
    company_id TEXT NOT NULL,
    artifact_id TEXT NOT NULL,
    -- 五类绑定：工作包修订、输入基线、产物修订、验证规则、目标环境（+ 合并组合）。
    work_package_revision TEXT NOT NULL,
    input_baseline TEXT NOT NULL,
    artifact_revision TEXT NOT NULL,
    verification_rules TEXT NOT NULL,
    target_environment TEXT NOT NULL,
    merge_combination TEXT,
    decision TEXT NOT NULL CHECK (decision IN ('accepted', 'auto_accepted', 'requires_human')),
    actor_id TEXT NOT NULL,
    reason TEXT NOT NULL DEFAULT '',
    evidence_refs TEXT NOT NULL DEFAULT '[]',
    accepted_at_ms INTEGER NOT NULL,
    superseded_by TEXT
);
CREATE INDEX IF NOT EXISTS idx_acceptances_artifact
    ON acceptances(artifact_id, accepted_at_ms);
CREATE INDEX IF NOT EXISTS idx_acceptances_revision
    ON acceptances(artifact_revision);

CREATE TABLE IF NOT EXISTS decisions (
    decision_id TEXT PRIMARY KEY,
    company_id TEXT NOT NULL,
    actor_id TEXT NOT NULL,
    scope_company TEXT NOT NULL,
    scope_project TEXT,
    scope_issue TEXT,
    decision TEXT NOT NULL,
    reason TEXT NOT NULL DEFAULT '',
    evidence_refs TEXT NOT NULL DEFAULT '[]',
    revision_refs TEXT NOT NULL DEFAULT '[]',
    created_at_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_decisions_scope ON decisions(company_id, scope_issue, created_at_ms);

CREATE TABLE IF NOT EXISTS work_package_states (
    company_id TEXT NOT NULL,
    issue_uid TEXT NOT NULL,
    revision_id TEXT NOT NULL,
    required INTEGER NOT NULL DEFAULT 1,
    accepted_revision TEXT,
    running INTEGER NOT NULL DEFAULT 0,
    blockers TEXT NOT NULL DEFAULT '[]',
    PRIMARY KEY (issue_uid, revision_id)
);

CREATE TABLE IF NOT EXISTS manifests (
    uid TEXT PRIMARY KEY,
    company_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision >= 1),
    milestone TEXT NOT NULL DEFAULT '',
    artifact_revision_refs TEXT NOT NULL DEFAULT '[]',
    acceptance_refs TEXT NOT NULL DEFAULT '[]',
    signature_ref TEXT,
    created_at_ms INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS publications (
    uid TEXT PRIMARY KEY,
    company_id TEXT NOT NULL,
    manifest_id TEXT NOT NULL,
    target TEXT NOT NULL,
    external_id TEXT,
    effect_id TEXT NOT NULL,
    state TEXT NOT NULL,
    observed_at_ms INTEGER
);

-- 修订内容不可变（§21.1）：改写或删除由数据库直接拒绝，而不是仅靠应用层约定。
CREATE TRIGGER IF NOT EXISTS artifact_revisions_no_update
BEFORE UPDATE ON artifact_revisions
BEGIN
    SELECT RAISE(ABORT, 'artifact revision is immutable');
END;

CREATE TRIGGER IF NOT EXISTS artifact_revisions_no_delete
BEFORE DELETE ON artifact_revisions
BEGIN
    SELECT RAISE(ABORT, 'artifact revision is immutable');
END;

-- 已记录的验证结论与接受记录同为追加式历史：不原地改写。
CREATE TRIGGER IF NOT EXISTS acceptances_immutable
BEFORE UPDATE ON acceptances
WHEN NEW.acceptance_id <> OLD.acceptance_id
   OR NEW.artifact_revision <> OLD.artifact_revision
   OR NEW.decision <> OLD.decision
   OR NEW.actor_id <> OLD.actor_id
   OR NEW.accepted_at_ms <> OLD.accepted_at_ms
BEGIN
    SELECT RAISE(ABORT, 'acceptance record is append-only');
END;
