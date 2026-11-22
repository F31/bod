-- P8 §11 关联追加（3.4）：后来补入的内部工作包、目标关联与 expected_version。
-- 权威来源：V2.9.8 §11「关联与补证在现有单写事务内提交」——「后来补入的内部工作包 SHALL
-- 按期望修订追加关联，重试不重复建项、不覆盖外部来源」。
--
-- 只建一张表，其余两件事（目标关联、版本推进）落在既有表上，理由逐条写下来：
--
-- 1. **目标不需要新表**：`delivery_targets`（016）的身份索引就是
--    (company_id, tracking_id, customer_id, environment, deployment_unit)，
--    追加目标 = 按身份找，找不到才插入。加一张「目标关联表」会让同一件事有两个来源，
--    而两者一旦不一致，「这个客户交付到哪」就要两份记录各说一套。
-- 2. **内部工作包需要表**：`delivery_tracking` 上的 issue_uid / run_uid / attempt_id
--    是三根单值列，表达不了「先后补入两个工作包修订」。工作包修订是一串事实，
--    不是跟踪项的属性，所以单独成表、按修订号去重。
-- 3. **版本推进不改表结构**：`delivery_tracking.resource_version` 早已存在（016），
--    本段只是第一次给它语义：追加成功才 +1，纯重复不动它。
--    纯重复也 +1 的话，两个客户端会互相把对方的 expected_version 判过期，
--    于是谁都不敢重试——这正是「重试不重复建项」要避免的失败方式。

CREATE TABLE IF NOT EXISTS delivery_tracking_work_packages (
    company_id TEXT NOT NULL,
    tracking_id TEXT NOT NULL,
    -- 内部工作包修订号（`work_packages` 侧的修订标识）。空串无意义，先拒绝。
    work_package_revision TEXT NOT NULL CHECK (length(trim(work_package_revision)) > 0),
    -- 谁补的这条关联。责任归属，不允许空。
    linked_by TEXT NOT NULL CHECK (length(trim(linked_by)) > 0),
    linked_at_ms INTEGER NOT NULL,
    -- 主键即去重键：同一跟踪项下的同一修订只能有一条关联。
    -- 重复提交撞主键，由写入原语改成返回 `appended=false`，不报错也不新增。
    PRIMARY KEY (company_id, tracking_id, work_package_revision),
    FOREIGN KEY (company_id, tracking_id)
        REFERENCES delivery_tracking(company_id, tracking_id)
);

-- 反查：某个工作包修订被哪些交付引用过。回答「这次改动的交付影响面」用得上。
CREATE INDEX IF NOT EXISTS idx_delivery_work_packages_revision
    ON delivery_tracking_work_packages(company_id, work_package_revision, linked_at_ms);

-- 关联**不原地改写**：把已挂的修订改成另一个，会让「谁在什么时候挂的哪一版」
-- 变成一个从未发生过的说法。挂错了应当删掉重挂——删除是纠正，改写是伪造。
CREATE TRIGGER IF NOT EXISTS delivery_work_packages_no_update
BEFORE UPDATE ON delivery_tracking_work_packages
BEGIN
    SELECT RAISE(ABORT, 'delivery work package link is not editable in place');
END;
