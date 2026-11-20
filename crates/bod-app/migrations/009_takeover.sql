-- P2 任务 2.1 持久交接意图与阶段状态。
-- 权威来源：V2.9.6 §19.2 用持久化交接意图与阶段状态协调，不在事务内等待 Agent 总结；
-- 最终绑定以 expected_version 条件提交。只建表建索引，不含破坏性 DDL。

CREATE TABLE IF NOT EXISTS takeover_intents (
    company_id TEXT NOT NULL,
    -- 同一 Company 内幂等键：同键同内容复用，同键不同内容拒绝。
    takeover_key TEXT NOT NULL,
    intent_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    issue_id TEXT NOT NULL,
    work_package_revision TEXT NOT NULL,
    from_attempt_id TEXT NOT NULL,
    requested_by TEXT NOT NULL,
    reason TEXT,
    -- 旧执行的 fencing token：新 token 必须严格更大。
    baseline_fencing_token INTEGER,
    created_at TEXT NOT NULL,
    -- 阶段与总结状态分开：等待总结跨越事务边界，崩溃恢复要能一眼看出「正在等」。
    phase TEXT NOT NULL CHECK (phase IN (
        'requested', 'awaiting_summary', 'summary_received', 'draining', 'captured',
        'isolation_checked', 'admitted', 'attempt_created', 'restored', 'running', 'aborted'
    )),
    -- 乐观并发版本：每次成功写入 +1，条件提交的比较基准。
    version INTEGER NOT NULL,
    summary_state TEXT NOT NULL CHECK (summary_state IN (
        'not_requested', 'requested', 'received', 'abandoned'
    )),
    summary_requested_at TEXT,
    summary_snapshot_key TEXT,
    attempt_id TEXT,
    fencing_token INTEGER,
    abort_reason TEXT,
    -- 每阶段时间与结果，全程可查询；最后一条必须与 phase 列一致。
    history_json TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    recorded_at_ms INTEGER NOT NULL,
    PRIMARY KEY (company_id, takeover_key)
);
CREATE INDEX IF NOT EXISTS idx_takeover_intents_phase
    ON takeover_intents(company_id, phase, updated_at);
