-- CR-002 第 1 步：业务审计事件落库。
-- 权威来源：V2.9.8 §8「日志和审计保留关联 ID、授权依据、范围、决策与原因并脱敏」；
-- §9「业务审计回答『谁基于什么证据做了什么决定』，诊断日志供故障定位」。
--
-- 与 ledger_entries / decisions 的区别：这两张表记**业务事实**（预算变动、接受决定），
-- 本表记**判定过程**——包括被拒绝的动作。拒绝不产生业务事实，但必须可追溯，
-- 所以不能靠业务表反推。两者共用 correlation_id 串成一条链。
--
-- 只建表建索引与不可变 trigger，不含破坏性 DDL。

CREATE TABLE IF NOT EXISTS audit_events (
    -- 全库单调自增：审计的阅读顺序必须稳定，不能依赖毫秒时间戳（同毫秒并发写入是常态）。
    -- AUTOINCREMENT 保证即使将来发生删除也不复用编号（本表禁止删除，是双保险）。
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    company_id TEXT NOT NULL,
    -- 关联 ID：一次用户动作贯穿的链（入口解析 → 阶段校验 → 外部效果 → 回执）。
    -- 同一次动作的所有审计行共享此值，跨表关联也用它，别用时间戳对齐。
    correlation_id TEXT NOT NULL,
    -- 谁：可信身份来自已认证会话 / 设备 / 回执授权 / 执行绑定（§8），不是请求路径。
    actor_id TEXT NOT NULL,
    -- 做了什么：阶段动作名（`submit` / `accept` / `release` / `revoke` …）。
    action TEXT NOT NULL,
    -- 作用范围：公司必填；Project 与 Issue 按动作粒度可空
    -- （成员管理是公司级动作，范围批准发生在 Issue 之前）。
    scope_company TEXT NOT NULL,
    scope_project TEXT,
    scope_issue TEXT,
    -- 依据：命中的授权凭据（成员 / Grant / 指派 / 发布权）与判定所读的事实。
    -- 脱敏由写入侧负责（`audit_log::AuditEvent::validate` 拒收秘密形态），本表只存结果。
    basis TEXT NOT NULL,
    decision TEXT NOT NULL CHECK (decision IN ('allowed', 'denied')),
    -- 拒绝原因：放行时必须为 NULL。留空串会让「没有原因」与「原因为空」无法区分。
    reason TEXT,
    -- 业务时间（调用方给的时刻）与落库时间分开存：
    -- 恢复演练时重放的旧事件，业务时间应保持原值，落库时间才反映「何时被记下来」。
    decided_at TEXT NOT NULL,
    recorded_at_ms INTEGER NOT NULL,
    -- 库层再兜一次：绕过 Rust 校验的写入（外部 sqlite3、恢复脚本）同样不被接受。
    CHECK (
        (decision = 'denied' AND reason IS NOT NULL AND length(trim(reason)) > 0)
        OR (decision = 'allowed' AND reason IS NULL)
    )
);

-- 按关联 ID 读一条链；sequence 收尾保证同链内顺序稳定。
CREATE INDEX IF NOT EXISTS idx_audit_events_correlation
    ON audit_events(company_id, correlation_id, sequence);

-- §9 观测要按决策分布计数（拒绝率是暴露问题的第一手信号）。
CREATE INDEX IF NOT EXISTS idx_audit_events_decision
    ON audit_events(company_id, decision, recorded_at_ms);

-- 审计不可改写、不可删除。
-- 与 014 的 decisions trigger 同款理由：即使应用层只 INSERT，也要让「发现了也改不了」
-- 提前成「根本改不了」。绕过 trigger 的路径（从备份恢复、外部工具）由
-- 写入侧的顺序号连续性检查兜住。
CREATE TRIGGER IF NOT EXISTS audit_events_no_update
BEFORE UPDATE ON audit_events
BEGIN
    SELECT RAISE(ABORT, 'audit events are immutable');
END;

CREATE TRIGGER IF NOT EXISTS audit_events_no_delete
BEFORE DELETE ON audit_events
BEGIN
    SELECT RAISE(ABORT, 'audit events are immutable');
END;
