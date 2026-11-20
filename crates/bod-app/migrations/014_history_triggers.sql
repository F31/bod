-- P2 3.4 加固（2026-11-21）：decisions 此前只有「事后比对发现偏移」，没有库层阻止。
-- 生产路径只 INSERT（store.rs::insert_decision），没有任何 UPDATE/DELETE 语句，
-- 因此加不可变 trigger 不影响既有语义，只把「发现了也改不了」提前成「根本改不了」。
-- 与 ledger_entries / artifact_revisions 同款；历史连续比对仍保留，用于兜住
-- 绕过 trigger 的路径（从备份恢复、外部 sqlite3 直接改写、trigger 被删）。
CREATE TRIGGER IF NOT EXISTS decisions_no_update
BEFORE UPDATE ON decisions
BEGIN
    SELECT RAISE(ABORT, 'decision records are immutable');
END;

CREATE TRIGGER IF NOT EXISTS decisions_no_delete
BEFORE DELETE ON decisions
BEGIN
    SELECT RAISE(ABORT, 'decision records are immutable');
END;
