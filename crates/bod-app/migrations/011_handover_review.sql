-- P2 任务 3.3 检查点与接管确认。
-- 权威来源：V2.9.6 §39.11 I-65「检查点带游标，接管补读增量，不依赖旧经理在线或叙述正确」；
-- G-42「无旧经理总结、部分资料缺失、快照后新增员工结果，能够查询缺口和增量」；
-- manager-transfer「初始只读确认 MUST NOT 产生管理权，随后命令仍逐次验证当前委任」。
-- 只建表建索引，不含破坏性 DDL；不复制业务事实，检查点只是「某次只读核对看到的现场指纹」。

CREATE TABLE IF NOT EXISTS handover_checkpoints (
    company_id TEXT NOT NULL,
    -- 由 scope + 游标决定的宿主 id：同现场重复捕获落回同一行，不产生第二份检查点。
    checkpoint_id TEXT NOT NULL,
    scope_key TEXT NOT NULL,
    project_id TEXT NOT NULL,
    -- 事实游标：对排序后的 (kind, id, fingerprint) 行文本求 sha256；同游标即同现场。
    cursor TEXT NOT NULL,
    item_count INTEGER NOT NULL CHECK (item_count >= 0),
    -- 现场指纹清单（只含 kind/id/fingerprint，不含内容）；增量读取的唯一比较基准。
    items_json TEXT NOT NULL,
    -- 捕获时就已存在的缺口（无交接快照、部分快照、未对账执行、待人工验收）。
    gaps_json TEXT NOT NULL,
    captured_by TEXT NOT NULL,
    captured_at TEXT NOT NULL,
    recorded_at_ms INTEGER NOT NULL,
    PRIMARY KEY (company_id, checkpoint_id)
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_handover_checkpoints_cursor
    ON handover_checkpoints(company_id, scope_key, cursor);

CREATE TABLE IF NOT EXISTS takeover_confirmations (
    company_id TEXT NOT NULL,
    -- 幂等键（宿主提供）：同键同内容返回原行，同键不同内容拒绝。
    confirmation_id TEXT NOT NULL,
    scope_key TEXT NOT NULL,
    checkpoint_id TEXT NOT NULL,
    -- 确认者复核的那份检查点游标；与 reviewed_cursor 不同即「确认时现场已前进」。
    checkpoint_cursor TEXT NOT NULL,
    reviewed_cursor TEXT NOT NULL,
    -- 确认所绑定的委任与 epoch：epoch 前进后本行只变成 Superseded，不会变成权力。
    appointment_id TEXT NOT NULL,
    authority_epoch INTEGER NOT NULL CHECK (authority_epoch > 0),
    decision TEXT NOT NULL CHECK (decision IN ('confirmed', 'blocked')),
    confirmed_by TEXT NOT NULL,
    note TEXT NOT NULL,
    -- 确认时仍存在的缺口与相对检查点的增量计数：记录不等于已解决。
    open_items_json TEXT NOT NULL,
    delta_json TEXT NOT NULL,
    confirmed_at TEXT NOT NULL,
    recorded_at_ms INTEGER NOT NULL,
    PRIMARY KEY (company_id, confirmation_id)
);
CREATE INDEX IF NOT EXISTS idx_takeover_confirmations_scope
    ON takeover_confirmations(company_id, scope_key, authority_epoch, recorded_at_ms);
