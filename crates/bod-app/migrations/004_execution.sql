-- P0 D4 执行端协议持久化迁移（任务 5.1—5.5）
-- 权威来源：V2.9.6 §15.3—§15.6、§33 执行组（invocations / observations / capability 记录）。
-- 只创建表与索引，不含破坏性 DDL。执行端内部动作不在此建模，只记录宿主已观察范围。

-- 持久命令去重：接收记录必须先落盘再对外确认（§15.6）。
CREATE TABLE IF NOT EXISTS invocations (
    invocation_id TEXT PRIMARY KEY,
    company_id TEXT NOT NULL,
    run_id TEXT NOT NULL,
    work_package_revision TEXT NOT NULL,
    attempt_id TEXT NOT NULL,
    binding_id TEXT NOT NULL,
    fencing_token INTEGER NOT NULL,
    adapter_id TEXT NOT NULL,
    adapter_version TEXT NOT NULL,
    -- 规范化参数文本用于精确判定"同 ID 同参"；digest 仅供展示，不作为相等依据。
    params_canonical TEXT NOT NULL,
    params_digest TEXT NOT NULL,
    execution_id TEXT,
    state TEXT NOT NULL
        CHECK (state IN ('Accepted', 'Starting', 'Running', 'NeedsInput', 'NeedsAuth',
                         'Suspended', 'Cancelling', 'Completed', 'Failed', 'Cancelled', 'Unknown')),
    created_at_ms INTEGER NOT NULL,
    accepted_at_ms INTEGER,
    last_observed_at_ms INTEGER,
    retention_until_ms INTEGER,
    CHECK (fencing_token >= 0)
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_invocations_execution
    ON invocations(execution_id) WHERE execution_id IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_invocations_run ON invocations(run_id, state);
CREATE INDEX IF NOT EXISTS idx_invocations_retention ON invocations(retention_until_ms);

-- 每次 start 尝试的审计：回包丢失只记尝试，不新建 Attempt（I-04）。
CREATE TABLE IF NOT EXISTS start_audits (
    audit_id INTEGER PRIMARY KEY AUTOINCREMENT,
    invocation_id TEXT NOT NULL,
    outcome TEXT NOT NULL
        CHECK (outcome IN ('persisted', 'accepted', 'replayed', 'reply_lost',
                           'params_rejected', 'unknown')),
    detail TEXT,
    recorded_at_ms INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_start_audits_invocation
    ON start_audits(invocation_id, recorded_at_ms);

-- 观察 inbox：先持久接收再 ACK；主键即去重键（invocation + epoch + stream + sequence）。
CREATE TABLE IF NOT EXISTS observations (
    invocation_id TEXT NOT NULL,
    source_epoch INTEGER NOT NULL,
    stream_id TEXT NOT NULL,
    source_sequence INTEGER NOT NULL,
    observed_at_ms INTEGER NOT NULL,
    kind TEXT NOT NULL
        CHECK (kind IN ('progress', 'question', 'permission_request', 'artifact', 'usage',
                        'log', 'state_change', 'final_result', 'error')),
    payload TEXT NOT NULL,
    origin TEXT NOT NULL,
    truncated INTEGER NOT NULL DEFAULT 0,
    persisted_at_ms INTEGER NOT NULL,
    processed INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (invocation_id, source_epoch, stream_id, source_sequence),
    CHECK (source_sequence >= 1)
);
CREATE INDEX IF NOT EXISTS idx_observations_pending
    ON observations(invocation_id, processed, source_sequence);

-- 接收游标：只确认最高连续 sequence；buffered 保留乱序序号，重启后重建缺口状态。
CREATE TABLE IF NOT EXISTS observation_cursors (
    invocation_id TEXT NOT NULL,
    source_epoch INTEGER NOT NULL,
    stream_id TEXT NOT NULL,
    highest_contiguous INTEGER,
    retained_floor INTEGER,
    buffered TEXT NOT NULL DEFAULT '',
    updated_at_ms INTEGER NOT NULL,
    PRIMARY KEY (invocation_id, source_epoch, stream_id)
);

-- 能力声明与证据：每项保留来源与观测时间，证据等级不得高于来源（§5.3、§14.2）。
CREATE TABLE IF NOT EXISTS adapter_capabilities (
    adapter_id TEXT NOT NULL,
    adapter_version TEXT NOT NULL,
    capability TEXT NOT NULL,
    level TEXT NOT NULL CHECK (level IN ('supported', 'unsupported', 'unknown')),
    evidence_level TEXT NOT NULL
        CHECK (evidence_level IN ('self_declared', 'configuration_checked', 'probe_verified',
                                   'runtime_observed', 'independently_verified')),
    source TEXT NOT NULL
        CHECK (source IN ('self_declared', 'platform_probe', 'test_result', 'manual_override')),
    reason TEXT,
    observed_at_ms INTEGER NOT NULL,
    expires_at_ms INTEGER,
    PRIMARY KEY (adapter_id, adapter_version, capability)
);

-- 分项探测结果：保留 scope 与 TTL，失败/过期降级为 unknown，不空称成功。
CREATE TABLE IF NOT EXISTS probe_results (
    adapter_id TEXT NOT NULL,
    adapter_version TEXT NOT NULL,
    aspect TEXT NOT NULL
        CHECK (aspect IN ('environment', 'credentials', 'model', 'quota', 'workspace', 'network')),
    status TEXT NOT NULL CHECK (status IN ('ready', 'not_ready', 'unsupported', 'unknown')),
    reason TEXT NOT NULL,
    source TEXT NOT NULL
        CHECK (source IN ('self_declared', 'platform_probe', 'test_result', 'manual_override')),
    scope TEXT NOT NULL,
    observed_at_ms INTEGER NOT NULL,
    expires_at_ms INTEGER,
    PRIMARY KEY (adapter_id, adapter_version, aspect)
);
