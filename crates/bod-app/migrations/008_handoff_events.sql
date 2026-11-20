-- P2 任务 1.5 交接事件与幂等接受。
-- 权威来源：V2.9.6 §18.13 记录 requested/captured/validated/accepted/blocked/resumed 与原因来源；
-- 同一 accept 重复提交返回同一结果，不创建第二份执行或第二份包。
-- 只创建表与索引，不含破坏性 DDL。

CREATE TABLE IF NOT EXISTS handoff_events (
    company_id TEXT NOT NULL,
    handoff_id TEXT NOT NULL,
    -- 从 1 开始、连续、不可重排；同交接内唯一。
    seq INTEGER NOT NULL,
    event_id TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN (
        'requested', 'captured', 'validated', 'accepted', 'blocked', 'resumed'
    )),
    -- blocked 必有原因；captured/validated/accepted 必有证据引用。
    reason TEXT,
    -- 事件来源同时是能力边界：agent 不能写 accepted。
    actor_kind TEXT NOT NULL CHECK (actor_kind IN ('agent', 'host', 'human', 'policy')),
    actor_ref TEXT,
    occurred_at TEXT NOT NULL,
    evidence_ref TEXT,
    recorded_at_ms INTEGER NOT NULL,
    PRIMARY KEY (company_id, handoff_id, event_id),
    UNIQUE (company_id, handoff_id, seq)
);
CREATE INDEX IF NOT EXISTS idx_handoff_events_timeline
    ON handoff_events(company_id, handoff_id, seq);

-- 一个交接只能被接受一次：主键不含幂等键，重复/并发接受都无法插入第二行。
CREATE TABLE IF NOT EXISTS handoff_acceptances (
    company_id TEXT NOT NULL,
    handoff_id TEXT NOT NULL,
    acceptance_key TEXT NOT NULL,
    -- 同一幂等键配不同内容必须能被识别，不能返回第一次的结果蒙混过去。
    request_digest TEXT NOT NULL,
    accepted_event_id TEXT NOT NULL,
    accepted_seq INTEGER NOT NULL,
    snapshot_key TEXT NOT NULL,
    level TEXT NOT NULL CHECK (level IN ('L1', 'L2', 'L3')),
    accepted_at TEXT NOT NULL,
    -- 接受者身份：读回时还原成同一条 accepted 事件，不伪造来源。
    actor_kind TEXT NOT NULL,
    actor_ref TEXT,
    recorded_at_ms INTEGER NOT NULL,
    PRIMARY KEY (company_id, handoff_id)
);
