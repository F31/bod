-- P8 3.6 B1：§8.1 判定所需事实的库侧来源。
--
-- 001 的 `members` 是 **Company 级身份**（uid/company_id/kind/display_name），没有 project_id；
-- `grants` 有完整作用域；其余事实（项目成员、客户绑定、发布操作人、撤权修订）此前
-- **无表、也无处派生**。于是 `StageFacts` 的六组事实只有一组能装配，
-- 装配端口无处落脚（见 docs/reports/v299-spec-review-and-plan.md §2 轨 B）。
--
-- 本迁移只加四张表，不改任何既有表。三条刻意的取舍：
--
-- 1. **成员分两层，不合并**：`members` 仍是 Company 级身份，`project_members` 是
--    「这个主体在这个项目里有没有立场」。Project 是授权边界（§8.1 阶段二），
--    把 project_id 塞进 `members` 会让「同一个人换项目」变成两行同 uid 的冲突。
-- 2. **一个主体同一时刻只绑定一个客户**（`customer_bindings` 的主键决定）。
--    `StageFacts::customer_of` 只返回一个客户，若允许一对多，判定就会依赖行的顺序。
--    需要同时服务多个客户时用不同主体——客户是交付目标维度，不是数据所有权边界
--    （V2.9.9 产品勘误 1）。
-- 3. **撤权修订单独登记，不从 `active = 0` 的行反推**。`grants` 的 revision 是
--    「授予时的修订」，停用只改 active，不 bump revision；拿它当撤权修订会**偏旧**，
--    结果是本该重同步的离线客户端少重同步一次（`check_subscription` 的 `since < revoked_at`）。
--    宁可有明确的空值，也不要一个看起来有值但偏小的数。
--
-- 四张表都带 `revision`，都允许 `active` 变更：授权事实本来就会变，
-- 它们不是历史表。历史由 `audit_events`（015）承担，这里只存**当前事实**。

CREATE TABLE IF NOT EXISTS project_members (
    company_id TEXT NOT NULL,
    project_id TEXT NOT NULL,
    principal_id TEXT NOT NULL,
    -- 与 001 `members.kind` 同一套小写取值。注意 `issues.owner_kind` 存的是
    -- "User"/"Agent"（大写）——同一枚举在库里有两套拼写，跨表按文本比对会恒不相等。
    kind TEXT NOT NULL CHECK (kind IN ('user', 'agent')),
    -- 这条成员资格所依据的授权修订号。
    revision INTEGER NOT NULL CHECK (revision >= 1),
    active INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0, 1)),
    updated_at_ms INTEGER NOT NULL,
    PRIMARY KEY (company_id, project_id, principal_id),
    CHECK (length(trim(project_id)) > 0),
    CHECK (length(trim(principal_id)) > 0)
);
-- 装配按 (company, principal) 取，主键前缀是 (company, project)，需要单独的索引。
CREATE INDEX IF NOT EXISTS idx_project_members_principal
    ON project_members(company_id, principal_id, active);

CREATE TABLE IF NOT EXISTS customer_bindings (
    company_id TEXT NOT NULL,
    principal_id TEXT NOT NULL,
    -- 客户目标标识，与 `delivery_targets.customer_id` 同一取值空间。
    customer_id TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision >= 1),
    active INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0, 1)),
    updated_at_ms INTEGER NOT NULL,
    -- 一个主体同一时刻只有一个客户绑定：一对多会让 `customer_of` 的答案取决于行序。
    PRIMARY KEY (company_id, principal_id),
    CHECK (length(trim(principal_id)) > 0),
    CHECK (length(trim(customer_id)) > 0)
);

CREATE TABLE IF NOT EXISTS release_operators (
    company_id TEXT NOT NULL,
    principal_id TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision >= 1),
    active INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0, 1)),
    updated_at_ms INTEGER NOT NULL,
    PRIMARY KEY (company_id, principal_id),
    CHECK (length(trim(principal_id)) > 0)
);

-- 撤权修订：`principal_id` 为空表示公司级撤权（影响所有主体），非空只影响该主体。
-- 追加式：撤权发生过就不能改写，否则「按哪一版授权判的」会被追溯性地改掉。
CREATE TABLE IF NOT EXISTS authorization_revocations (
    company_id TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision >= 1),
    principal_id TEXT,
    revoked_at_ms INTEGER NOT NULL,
    -- 撤权依据（谁批的、为什么）。空串表示未记录，不用 NULL 表示「没写」。
    reason TEXT NOT NULL DEFAULT '',
    PRIMARY KEY (company_id, revision),
    CHECK (principal_id IS NULL OR length(trim(principal_id)) > 0)
);

CREATE TRIGGER IF NOT EXISTS authorization_revocations_no_update
BEFORE UPDATE ON authorization_revocations
BEGIN
    SELECT RAISE(ABORT, 'authorization revocation is append-only');
END;

CREATE TRIGGER IF NOT EXISTS authorization_revocations_no_delete
BEFORE DELETE ON authorization_revocations
BEGIN
    SELECT RAISE(ABORT, 'authorization revocation is append-only');
END;
