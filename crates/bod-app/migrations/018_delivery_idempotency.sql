-- P8 §11 去重语义分离（3.3）：幂等键管命令重放，来源身份管对象重复。
-- 权威来源：V2.9.8 §11「命令重放与对象重复的语义分开」；设计见 design.md D2 与 3.3 小节。
--
-- 两张表，各管一件事，**不允许合成一个键**（design D2）：
--
-- 1. `delivery_command_receipts` 管「同一次命令被重放」：键的作用域是
--    (company_id, idempotency_key)，**不含命令**。同一个键换命令也是冲突——
--    客户端拿同一个键发两条不同命令本身就是缺陷，静默接受会让「不同对象被当成重放」。
--    载荷指纹由内容派生（调用方自编的指纹不受信），同一键不同指纹一律拒绝。
-- 2. `delivery_tracking_sources` 管「不同命令导入同一对象」：身份唯一性在 017 的
--    五元组索引上，这里记的是「这个身份承载哪一条跟踪关系」。
--    主键 (company_id, source_id) 表达一条硬约束：**一个来源身份只承载一条跟踪关系**。
--    若要同一个 PR 支撑两个交付承诺，要改的是关系模型（多对多 + 显式意图），
--    不是在这里放宽唯一性——放宽之后「重复导入返回已有关系」就没有唯一对象可返回。
--
-- 回执是**历史**：改写回执等于让重放返回一个当时并不存在的结果，所以 UPDATE/DELETE
-- 由触发器拒绝。关联关系不是历史（挂错了要能纠正），因此不设 no-update 触发器。

CREATE TABLE IF NOT EXISTS delivery_command_receipts (
    company_id TEXT NOT NULL CHECK (length(trim(company_id)) > 0),
    -- 客户端给的幂等键。空串不允许：没有键就没有重放语义。
    idempotency_key TEXT NOT NULL CHECK (length(trim(idempotency_key)) > 0),
    -- 命令域（`import-delivery-source` 等）。参与冲突判定，不参与键的作用域。
    command TEXT NOT NULL CHECK (length(trim(command)) > 0),
    -- 载荷指纹：由命令载荷的内容派生（sha256:<64 hex>）。**不含幂等键本身**，
    -- 否则换了键就等于换了载荷，同键异载荷永远撞不上。
    payload_fingerprint TEXT NOT NULL CHECK (length(trim(payload_fingerprint)) > 0),
    -- 首次执行产出的对象种类。目前只有两种：跟踪项、候选（候选不建跟踪项）。
    result_kind TEXT NOT NULL CHECK (result_kind IN ('tracking', 'candidate')),
    result_id TEXT NOT NULL CHECK (length(trim(result_id)) > 0),
    -- 首次执行的响应体（规范 JSON）。重放要还给调用方**当时**的结果，
    -- 不是今天的状态：跟踪项可能已关闭、来源可能已补了修订。
    result_detail TEXT NOT NULL CHECK (length(trim(result_detail)) > 0),
    recorded_at_ms INTEGER NOT NULL,
    PRIMARY KEY (company_id, idempotency_key)
);

-- 运维视图：按命令看这段时间受理了多少条、其中多少是重放（重放不建回执行，
-- 只体现在 recorded_at_ms 的分布上，所以这里按命令 + 时间排）。
CREATE INDEX IF NOT EXISTS idx_delivery_command_receipts_command
    ON delivery_command_receipts(company_id, command, recorded_at_ms);

CREATE TRIGGER IF NOT EXISTS delivery_command_receipts_no_update
BEFORE UPDATE ON delivery_command_receipts
BEGIN
    SELECT RAISE(ABORT, 'delivery command receipt is append-only');
END;

CREATE TRIGGER IF NOT EXISTS delivery_command_receipts_no_delete
BEFORE DELETE ON delivery_command_receipts
BEGIN
    SELECT RAISE(ABORT, 'delivery command receipt is append-only');
END;

-- 来源身份 → 跟踪关系。这一行就是 §11 说的「来源关系」：
-- 重复导入同一来源返回它，而不是再建一个跟踪项。
CREATE TABLE IF NOT EXISTS delivery_tracking_sources (
    company_id TEXT NOT NULL,
    source_id TEXT NOT NULL,
    tracking_id TEXT NOT NULL,
    -- 谁把这条来源挂到这个跟踪项上。责任归属，不允许空。
    linked_by TEXT NOT NULL CHECK (length(trim(linked_by)) > 0),
    linked_at_ms INTEGER NOT NULL,
    PRIMARY KEY (company_id, source_id),
    FOREIGN KEY (company_id, source_id)
        REFERENCES delivery_sources(company_id, source_id),
    FOREIGN KEY (company_id, tracking_id)
        REFERENCES delivery_tracking(company_id, tracking_id)
);

-- 一条跟踪项可以挂多个来源（同一需求可能涉及 PR 与 release 两个对象）。
CREATE INDEX IF NOT EXISTS idx_delivery_tracking_sources_tracking
    ON delivery_tracking_sources(company_id, tracking_id, linked_at_ms);
