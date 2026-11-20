-- P2 任务 1.4 交接快照（部分快照可持久登记）。
-- 权威来源：V2.9.6 §18.11 尽力快照带 missing/来源/完整性状态且可持久登记，§18.10 版本绑定。
-- 只创建表与索引，不含破坏性 DDL。

CREATE TABLE IF NOT EXISTS handoff_snapshots (
    company_id TEXT NOT NULL,
    -- company_id + snapshot_seq 的合成键；同序号的第二次登记不得覆盖第一次的现场。
    snapshot_key TEXT NOT NULL,
    project_id TEXT NOT NULL,
    issue_id TEXT NOT NULL,
    work_package_revision TEXT NOT NULL,
    source_attempt_id TEXT NOT NULL,
    level TEXT NOT NULL CHECK (level IN ('L1', 'L2', 'L3')),
    -- agent_submitted / host_reconstructed / crash_recovered / budget_exhausted / force_stopped
    source TEXT NOT NULL,
    -- complete / partial；由 missing 与完整性状态推导，写入供人工筛选。
    completeness TEXT NOT NULL CHECK (completeness IN ('complete', 'partial')),
    missing_json TEXT NOT NULL DEFAULT '[]',
    integrity_json TEXT NOT NULL DEFAULT '[]',
    -- 版本绑定：只含实际输入/工作包/现场 revision 与 snapshot_seq，不含标题评论等元数据。
    input_revision TEXT NOT NULL,
    scene_revision TEXT NOT NULL,
    snapshot_seq INTEGER NOT NULL,
    -- 仅当 payload 片段完整性已核验时非空；摘要存在不意味着内容仍可恢复。
    payload_digest TEXT,
    captured_at TEXT NOT NULL,
    registered_at_ms INTEGER NOT NULL,
    PRIMARY KEY (company_id, snapshot_key)
);
CREATE INDEX IF NOT EXISTS idx_handoff_snapshots_project
    ON handoff_snapshots(company_id, project_id, snapshot_seq);
CREATE INDEX IF NOT EXISTS idx_handoff_snapshots_partial
    ON handoff_snapshots(company_id, completeness, snapshot_seq);
