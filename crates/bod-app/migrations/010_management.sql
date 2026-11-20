-- P2 任务 3.1 管理委任、作用域头与命令审计。
-- 权威来源：V2.9.6 §5.9（ManagementAppointment 字段与状态）、§19.8（每个管理 scope 最多一个
-- 当前 Active 主委任、epoch 单调递增、期望版本条件提交）、§31.10（管理凭据绑定 Appointment /
-- authority_epoch / scope / 命令集合 / 时效 / audience）、§33.12（management_appointments、
-- management_scope_heads、management_commands）。只建表建索引，不含破坏性 DDL。

CREATE TABLE IF NOT EXISTS management_appointments (
    company_id TEXT NOT NULL,
    appointment_id TEXT NOT NULL,
    -- 作用域键：`project:<id>` 或 `issue:<project>/<root-issue>`，与 scope 一一对应。
    scope_key TEXT NOT NULL,
    manager_agent_id TEXT NOT NULL,
    grant_ref TEXT NOT NULL,
    -- 由宿主按当前头指针 +1 分配，Agent 不能自选（§19.3）。唯一约束防止 epoch 复用。
    authority_epoch INTEGER NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('active', 'suspended', 'revoked', 'expired')),
    appointed_by TEXT NOT NULL,
    appointed_at TEXT NOT NULL,
    expires_at TEXT,
    -- 管理凭据的受众：与宿主受众一致才接受。
    audience TEXT NOT NULL,
    -- Grant 允许的管理命令；治理命令（任命/撤任）不允许出现在这里。
    commands_json TEXT NOT NULL,
    -- 委任行自身版本：状态变更用 `WHERE version = expected` 条件更新。
    version INTEGER NOT NULL,
    recorded_at_ms INTEGER NOT NULL,
    PRIMARY KEY (company_id, appointment_id),
    -- 同一 scope 同一 epoch 只允许一个委任：epoch 复用直接冲突。
    UNIQUE (company_id, scope_key, authority_epoch)
);

-- 「每个管理 scope 最多一个当前 Active 主委任」由部分唯一索引在库层保证，
-- 不依赖调用方先查后写；并发任命第二个 Active 会在此失败。
CREATE UNIQUE INDEX IF NOT EXISTS ux_management_appointments_one_active
    ON management_appointments(company_id, scope_key) WHERE status = 'active';

CREATE INDEX IF NOT EXISTS idx_management_appointments_scope
    ON management_appointments(company_id, scope_key, authority_epoch);

CREATE TABLE IF NOT EXISTS management_scope_heads (
    company_id TEXT NOT NULL,
    scope_key TEXT NOT NULL,
    current_appointment_id TEXT NOT NULL,
    -- 单调递增且不回退：写入只能取当前值 +1。
    authority_epoch INTEGER NOT NULL,
    -- 资源版本：每个状态或指针变化 +1；命令复核与缓存失效都以它为准。
    version INTEGER NOT NULL,
    updated_at TEXT NOT NULL,
    recorded_at_ms INTEGER NOT NULL,
    PRIMARY KEY (company_id, scope_key)
);

CREATE TABLE IF NOT EXISTS management_commands (
    company_id TEXT NOT NULL,
    -- 稳定命令键：同键同语义重放返回原结果，同键不同参数拒绝（§19.8）。
    command_id TEXT NOT NULL,
    scope_key TEXT NOT NULL,
    appointment_id TEXT,
    authority_epoch INTEGER,
    command TEXT NOT NULL,
    outcome TEXT NOT NULL CHECK (outcome IN ('accepted', 'rejected')),
    reason_code TEXT,
    reason TEXT,
    at TEXT NOT NULL,
    recorded_at_ms INTEGER NOT NULL,
    PRIMARY KEY (company_id, command_id)
);

CREATE INDEX IF NOT EXISTS idx_management_commands_scope
    ON management_commands(company_id, scope_key, at);
