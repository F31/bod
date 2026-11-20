-- P2 3.4：历史成果与费用连续（I-61）。
--
-- 本迁移只做两件事：
-- 1. 给 `acceptances` 补一段缺失的不可变约束。005 的 `acceptances_immutable` 只冻结
--    id / artifact_revision / decision / actor_id / accepted_at_ms，五元组里的
--    work_package_revision、input_baseline、verification_rules、target_environment、
--    merge_combination 与 reason、evidence_refs 仍可 UPDATE——这正是「新经理追溯改写旧接受」
--    在数据库层的实际缺口。这里新增独立 trigger 补齐，不改写 005 的历史定义。
-- 2. 建 `history_disputes`：争议只能标「待核查」，被争议的行一字不改。

CREATE TABLE IF NOT EXISTS history_disputes (
    dispute_id TEXT PRIMARY KEY,
    company_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    fact_kind TEXT NOT NULL
        CHECK (fact_kind IN ('artifact', 'revision', 'manifest', 'acceptance', 'ledger', 'decision')),
    fact_id TEXT NOT NULL,
    -- 提出争议时看到的事实指纹。后续指纹变化说明争议对象已被改动，不能当结论用。
    fact_fingerprint TEXT NOT NULL,
    reason TEXT NOT NULL CHECK (length(trim(reason)) > 0),
    evidence_refs TEXT NOT NULL DEFAULT '[]',
    raised_by TEXT NOT NULL CHECK (length(trim(raised_by)) > 0),
    state TEXT NOT NULL DEFAULT 'under_review'
        CHECK (state IN ('under_review', 'upheld', 'overturned')),
    resolution TEXT,
    resolved_by TEXT,
    resolved_at_ms INTEGER,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    -- 未结案⇔没有结案时间：两者不能各说各话。
    CHECK ((state = 'under_review') = (resolved_at_ms IS NULL))
);

CREATE INDEX IF NOT EXISTS idx_history_disputes_scope
    ON history_disputes(company_id, project_id, state);
CREATE INDEX IF NOT EXISTS idx_history_disputes_fact
    ON history_disputes(fact_kind, fact_id);

-- 接受记录的五元组与理由/证据写入后冻结。
CREATE TRIGGER IF NOT EXISTS acceptances_binding_immutable
BEFORE UPDATE ON acceptances
WHEN NEW.work_package_revision <> OLD.work_package_revision
   OR NEW.input_baseline <> OLD.input_baseline
   OR NEW.verification_rules <> OLD.verification_rules
   OR NEW.target_environment <> OLD.target_environment
   OR NEW.merge_combination IS NOT OLD.merge_combination
   OR NEW.reason <> OLD.reason
   OR NEW.evidence_refs <> OLD.evidence_refs
BEGIN
    SELECT RAISE(ABORT, 'acceptance binding is immutable');
END;

-- 取代指针只能从 NULL 前进一次，不能改写或清空（向前只进）。
CREATE TRIGGER IF NOT EXISTS acceptances_supersede_monotonic
BEFORE UPDATE ON acceptances
WHEN OLD.superseded_by IS NOT NULL
 AND (NEW.superseded_by IS NULL OR NEW.superseded_by <> OLD.superseded_by)
BEGIN
    SELECT RAISE(ABORT, 'acceptance supersede pointer is monotonic');
END;

-- 争议的对象、理由与提出人冻结：只有状态与结案字段可以前进。
CREATE TRIGGER IF NOT EXISTS history_disputes_subject_immutable
BEFORE UPDATE ON history_disputes
WHEN NEW.fact_kind <> OLD.fact_kind
   OR NEW.fact_id <> OLD.fact_id
   OR NEW.fact_fingerprint <> OLD.fact_fingerprint
   OR NEW.reason <> OLD.reason
   OR NEW.raised_by <> OLD.raised_by
BEGIN
    SELECT RAISE(ABORT, 'dispute subject is immutable');
END;

-- 已结案的争议不能改判或重开（要改结论只能另开一条争议）。
CREATE TRIGGER IF NOT EXISTS history_disputes_state_forward
BEFORE UPDATE ON history_disputes
WHEN OLD.state <> 'under_review' AND NEW.state <> OLD.state
BEGIN
    SELECT RAISE(ABORT, 'dispute state is final');
END;

CREATE TRIGGER IF NOT EXISTS history_disputes_no_delete
BEFORE DELETE ON history_disputes
BEGIN
    SELECT RAISE(ABORT, 'dispute records are append-only');
END;
