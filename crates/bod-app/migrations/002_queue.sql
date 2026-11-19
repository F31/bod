-- P0 D2 持久队列与 outbox 迁移（任务 3.1—3.4）
-- 权威来源：V2.9.6 §9.10—9.12、§33.13、§33.15 最小 SQL 基线。
-- 本脚本只创建表与索引，不含 DROP/DELETE 等破坏性 DDL；破坏性变更走独立迁移。
-- wake_receipts 与 work_queue 的语义约束在 001_initial.sql 的 leases 之外独立实现，
-- 不混淆历史幂等（wake_receipts）与活跃代数合并（work_queue）。

CREATE TABLE IF NOT EXISTS work_queue (
    company_id TEXT NOT NULL,
    queue_key TEXT NOT NULL,
    state TEXT NOT NULL DEFAULT 'idle'
        CHECK (state IN ('idle', 'pending', 'running', 'blocked', 'dead', 'cancelled')),
    wanted_generation INTEGER NOT NULL DEFAULT 0,
    claimed_generation INTEGER NOT NULL DEFAULT 0,
    completed_generation INTEGER NOT NULL DEFAULT 0,
    next_due_at_ms INTEGER NOT NULL DEFAULT 0,
    attempts INTEGER NOT NULL DEFAULT 0,
    lease_owner TEXT,
    lease_token INTEGER NOT NULL DEFAULT 0,
    lease_expires_at_ms INTEGER,
    last_error TEXT,
    blocked_reason TEXT,
    PRIMARY KEY (company_id, queue_key),
    CHECK (0 <= completed_generation
        AND completed_generation <= claimed_generation
        AND claimed_generation <= wanted_generation)
);
CREATE INDEX IF NOT EXISTS idx_work_queue_due ON work_queue(company_id, state, next_due_at_ms);

CREATE TABLE IF NOT EXISTS wake_receipts (
    company_id TEXT NOT NULL,
    producer_id TEXT NOT NULL,
    idempotency_key TEXT NOT NULL,
    request_digest TEXT NOT NULL,
    queue_key TEXT NOT NULL,
    generation INTEGER NOT NULL,
    accepted_at_ms INTEGER NOT NULL,
    retain_until_ms INTEGER NOT NULL,
    schedule_id TEXT,
    occurrence_id TEXT,
    PRIMARY KEY (company_id, producer_id, idempotency_key),
    FOREIGN KEY (company_id, queue_key) REFERENCES work_queue(company_id, queue_key)
);
CREATE INDEX IF NOT EXISTS idx_wake_receipts_retain ON wake_receipts(retain_until_ms);

CREATE TABLE IF NOT EXISTS outbox_deliveries (
    event_or_command_id TEXT NOT NULL,
    destination_id TEXT NOT NULL,
    company_id TEXT NOT NULL,
    queue_key TEXT NOT NULL,
    request_digest TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'prepared'
        CHECK (status IN ('prepared', 'dispatched', 'confirmed', 'failed', 'unknown')),
    attempts INTEGER NOT NULL DEFAULT 0,
    next_due_at_ms INTEGER NOT NULL DEFAULT 0,
    result_ref TEXT,
    created_at_ms INTEGER NOT NULL,
    PRIMARY KEY (event_or_command_id, destination_id)
);
CREATE INDEX IF NOT EXISTS idx_outbox_due ON outbox_deliveries(status, next_due_at_ms);
