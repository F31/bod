-- P2 3.5：专项委任边界与组织元数据分离。
--
-- 1. `issue_hierarchy`：专项委任的作用域是「根 Issue 子树」，而 P0 的 `issues` 没有父指针。
--    这里单独登记父子关系，不改 `issues` 表结构、不改 `bod-core::Issue`（P0/P1 的既有
--    构造点全部保持原样）。层级一次写定：改父节点会改变「专项边界归属」，因此 UPDATE/DELETE
--    都被拒绝，重建层级只能靠治理流程（本期不提供）。
-- 2. `agent_org_metadata`：reportsTo 是可更新的组织元数据，独立存放，不与 Run/租约/权限
--    共用任何字段。自指在 CHECK 层就拒绝。

CREATE TABLE IF NOT EXISTS issue_hierarchy (
    company_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    issue_uid TEXT NOT NULL,
    parent_issue_uid TEXT NOT NULL,
    created_at_ms INTEGER NOT NULL,
    PRIMARY KEY (company_id, issue_uid),
    CHECK (issue_uid <> parent_issue_uid)
);

CREATE INDEX IF NOT EXISTS idx_issue_hierarchy_parent
    ON issue_hierarchy(company_id, project_id, parent_issue_uid);

CREATE TRIGGER IF NOT EXISTS issue_hierarchy_immutable
BEFORE UPDATE ON issue_hierarchy
BEGIN
    SELECT RAISE(ABORT, 'issue hierarchy is immutable');
END;

CREATE TRIGGER IF NOT EXISTS issue_hierarchy_no_delete
BEFORE DELETE ON issue_hierarchy
BEGIN
    SELECT RAISE(ABORT, 'issue hierarchy is immutable');
END;

CREATE TABLE IF NOT EXISTS agent_org_metadata (
    company_id TEXT NOT NULL,
    agent_id TEXT NOT NULL,
    reports_to TEXT,
    updated_at_ms INTEGER NOT NULL,
    updated_by TEXT NOT NULL,
    PRIMARY KEY (company_id, agent_id),
    CHECK (reports_to IS NULL OR reports_to <> agent_id)
);
