//! 导入的幂等语义（P8 3.3）：同键重放、异载荷冲突、失败不污染键、审计同事务。

use super::*;

/// §11 的并发导入用例：两人各用一个键导入同一个 PR，
/// 只产生**一个跟踪项与一条来源关系**；后到者拿到已有关系。
///
/// 两个键各自留一条回执——它们确实是两条命令，去重发生在来源身份上，不在键上。
#[test]
fn two_keys_importing_one_source_yield_one_tracking_item() {
    let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
    let first = with_uow(&mut store, |uow| {
        uow.import_delivery_source(&import_command("k-alice", "dt-1", "src-1"), 1_000)
    })
    .expect("首次导入应成功");
    assert!(first.created_tracking && first.created_source && !first.replayed);

    let second = with_uow(&mut store, |uow| {
        uow.import_delivery_source(&import_command("k-bob", "dt-2", "src-2"), 1_100)
    })
    .expect("重复导入应成功而不是报错");

    assert_eq!(
        second.tracking_id.as_deref(),
        Some("dt-1"),
        "后到者必须拿到已有跟踪项，不能建第二个"
    );
    assert_eq!(
        second.source_id.as_deref(),
        Some("src-1"),
        "五元组命中同一来源身份"
    );
    assert!(!second.created_tracking && !second.created_source);
    assert!(!second.replayed, "换键不是重放：这是另一条命令");

    assert_eq!(count_rows(&store, "delivery_tracking"), 1);
    assert_eq!(count_rows(&store, "delivery_tracking_sources"), 1);
    assert_eq!(count_rows(&store, "delivery_sources"), 1);
    assert_eq!(count_rows(&store, "delivery_command_receipts"), 2);
}

/// 同键同载荷：重放首次结果，**不写第二遍**。
#[test]
fn same_key_and_payload_replays_without_writing() {
    let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
    let command = import_command("k-1", "dt-1", "src-1");
    let first = with_uow(&mut store, |uow| {
        uow.import_delivery_source(&command, 1_000)
    })
    .expect("首次导入应成功");
    let snapshot: Vec<i64> = [
        "delivery_tracking",
        "delivery_sources",
        "delivery_tracking_sources",
        "delivery_command_receipts",
    ]
    .iter()
    .map(|table| count_rows(&store, table))
    .collect();

    let again = with_uow(&mut store, |uow| {
        uow.import_delivery_source(&command, 1_500)
    })
    .expect("重放应成功");
    assert!(again.replayed, "同键同载荷必须是重放");
    assert_eq!(again.tracking_id, first.tracking_id);
    // created_* 描述首次执行的事实，不因重放而改写。
    assert!(again.created_tracking && again.created_source);

    let after: Vec<i64> = [
        "delivery_tracking",
        "delivery_sources",
        "delivery_tracking_sources",
        "delivery_command_receipts",
    ]
    .iter()
    .map(|table| count_rows(&store, table))
    .collect();
    assert_eq!(snapshot, after, "重放不得写入任何一行");
}

/// 同键异载荷：冲突，且**什么都不写**（不是「覆盖上一次」）。
#[test]
fn same_key_with_other_payload_conflicts_and_writes_nothing() {
    let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
    let first = import_command("k-1", "dt-1", "src-1");
    with_uow(&mut store, |uow| uow.import_delivery_source(&first, 1_000)).expect("首次导入应成功");

    let mut retitled = first.clone();
    retitled.title = "改过的标题".to_string();
    let err = with_uow(&mut store, |uow| {
        uow.import_delivery_source(&retitled, 1_100)
    })
    .expect_err("同键异载荷必须冲突");
    assert!(
        matches!(
            err,
            StoreError::Idempotency(IdempotencyError::KeyReusedWithOtherPayload { .. })
        ),
        "{err:?}"
    );
    assert_eq!(count_rows(&store, "delivery_tracking"), 1);
    assert_eq!(count_rows(&store, "delivery_command_receipts"), 1);
}

/// 失败的导入不污染键：修正载荷后用**同一个键**重试应当是首次执行。
///
/// 这是「回执与业务写入同事务」的直接后果。若先写回执再干活，
/// 这里的重试会撞上自己伪造的冲突，然后人就再也不敢用那个键了。
#[test]
fn failed_import_does_not_poison_the_key() {
    let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
    let mut broken = import_command("k-1", "dt-1", "src-1");
    broken.object_type = None; // 有稳定 ID 却没类型：调用方漏字段
    let err = with_uow(&mut store, |uow| uow.import_delivery_source(&broken, 1_000))
        .expect_err("缺对象类型必须报错");
    assert!(
        matches!(err, StoreError::Source(SourceError::MissingObjectType)),
        "{err:?}"
    );
    assert_eq!(
        count_rows(&store, "delivery_command_receipts"),
        0,
        "失败的导入不得留下回执"
    );
    assert_eq!(count_rows(&store, "delivery_sources"), 0);

    let fixed = import_command("k-1", "dt-1", "src-1");
    let outcome = with_uow(&mut store, |uow| uow.import_delivery_source(&fixed, 1_100))
        .expect("修正后同键重试应成功");
    assert!(!outcome.replayed, "此前没有成功执行过，这不是重放");
    assert!(outcome.created_tracking && outcome.created_source);
}

/// 只有定位：进候选，**不建跟踪项**。候选没有稳定身份，撑不起覆盖结论。
#[test]
fn locator_only_import_creates_a_candidate_not_a_tracking_item() {
    let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
    let mut command = import_command("k-1", "dt-1", "src-1");
    command.provider_object_id = None;
    command.locator = "https://github.com/acme/web/pull/318".to_string();

    let outcome = with_uow(&mut store, |uow| {
        uow.import_delivery_source(&command, 1_000)
    })
    .expect("只有定位应进候选而不是报错");
    assert_eq!(outcome.candidate_id.as_deref(), Some("src-1-cand"));
    assert!(outcome.created_candidate);
    assert!(!outcome.created_tracking);
    assert!(outcome.tracking_id.is_none());
    assert_eq!(count_rows(&store, "delivery_tracking"), 0);
    assert_eq!(count_rows(&store, "delivery_source_candidates"), 1);

    let receipt = store
        .find_command_receipt("company-default", "k-1")
        .expect("应能读回执")
        .expect("回执应存在");
    assert_eq!(receipt.result_kind, ResultKind::Candidate);
    assert_eq!(receipt.result_id, "src-1-cand");
}

/// 导入到已存在的跟踪项：不建新项，只挂关系。
#[test]
fn import_can_attach_to_an_existing_tracking_item() {
    let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
    with_uow(&mut store, |uow| {
        uow.insert_delivery_tracking(&delivery_tracking("company-default", "dt-1"))
    })
    .expect("跟踪项应可写入");

    let outcome = with_uow(&mut store, |uow| {
        uow.import_delivery_source(&import_command("k-1", "dt-1", "src-1"), 1_000)
    })
    .expect("导入到已有跟踪项应成功");
    assert_eq!(outcome.tracking_id.as_deref(), Some("dt-1"));
    assert!(!outcome.created_tracking, "跟踪项本来就在，不算本次新建");
    assert!(outcome.created_source);
    assert_eq!(count_rows(&store, "delivery_tracking"), 1);
    assert_eq!(count_rows(&store, "delivery_tracking_sources"), 1);
}

/// 一个来源身份只承载一条跟踪关系：换一条跟踪项挂要明确拒绝，重复挂同一条则幂等。
#[test]
fn a_source_cannot_be_linked_to_a_second_tracking_item() {
    let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
    with_uow(&mut store, |uow| {
        uow.insert_delivery_tracking(&delivery_tracking("company-default", "dt-1"))?;
        uow.insert_delivery_tracking(&delivery_tracking("company-default", "dt-2"))?;
        uow.import_delivery_source(&import_command("k-1", "dt-1", "src-1"), 1_000)
            .map(|_| ())
    })
    .expect("首次导入应成功");

    let err = with_uow(&mut store, |uow| {
        uow.link_delivery_tracking_source("company-default", "src-1", "dt-2", "bob", 1_100)
    })
    .expect_err("改挂必须被拒绝");
    assert!(
        matches!(
            err,
            StoreError::Source(SourceError::SourceAlreadyLinked { .. })
        ),
        "{err:?}"
    );

    // 重复挂同一条：幂等，不新增行也不报错。
    let again = with_uow(&mut store, |uow| {
        uow.link_delivery_tracking_source("company-default", "src-1", "dt-1", "bob", 1_200)
    })
    .expect("重复挂同一条应幂等");
    assert!(!again.created);
    assert_eq!(count_rows(&store, "delivery_tracking_sources"), 1);
}

/// 跨项目的来源不得挂到本项目的交付上（复合外键读不出「项目不同」）。
#[test]
fn cross_project_link_is_refused() {
    let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
    let mut other_project = delivery_tracking("company-default", "dt-2");
    other_project.project_id = "project-2".to_string();
    with_uow(&mut store, |uow| {
        uow.insert_delivery_tracking(&other_project)?;
        uow.import_delivery_source(&import_command("k-1", "dt-1", "src-1"), 1_000)
            .map(|_| ())
    })
    .expect("首次导入应成功");

    let err = with_uow(&mut store, |uow| {
        uow.link_delivery_tracking_source("company-default", "src-1", "dt-2", "bob", 1_100)
    })
    .expect_err("跨项目挂载必须被拒绝");
    assert!(
        matches!(
            err,
            StoreError::Source(SourceError::SourceProjectMismatch { .. })
        ),
        "{err:?}"
    );
}

/// 回执不可改写：重放必须返回**当时**的结果，改写等于伪造历史。
#[test]
fn command_receipts_are_append_only() {
    let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
    with_uow(&mut store, |uow| {
        uow.import_delivery_source(&import_command("k-1", "dt-1", "src-1"), 1_000)
            .map(|_| ())
    })
    .expect("首次导入应成功");

    let update = store.execute_batch_for_test(
        "UPDATE delivery_command_receipts SET result_id = 'dt-9' WHERE idempotency_key = 'k-1'",
    );
    assert!(update.is_err(), "改写回执必须被拒绝");
    let delete = store.execute_batch_for_test(
        "DELETE FROM delivery_command_receipts WHERE idempotency_key = 'k-1'",
    );
    assert!(delete.is_err(), "删除回执必须被拒绝");
}

// ---------------------------------------------------------------------------

/// 导入的审计与业务写入同事务：读得到的审计必然对应一次真的发生过的写入。
#[test]
fn import_writes_audit_in_the_same_transaction() {
    let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
    let outcome = with_uow(&mut store, |uow| {
        uow.import_delivery_source(&import_command("k-audit", "dt-9", "src-9"), 1_000)
    })
    .expect("导入应成功");
    assert_eq!(outcome.tracking_id.as_deref(), Some("dt-9"));

    let rows = store
        .list_audit_events("company-default", "req-import")
        .expect("应可读回");
    assert_eq!(rows.len(), 1, "一次导入只留一条审计");
    let event = &rows[0].event;
    assert_eq!(event.action, AuditAction::ImportSource);
    assert_eq!(event.decision, AuditDecision::Allowed);
    assert_eq!(event.basis, "import-source:source_identity");
    assert_eq!(event.actor.as_str(), "alice");
    assert_eq!(
        event.scope.project_id.as_ref().map(DomainId::as_str),
        Some("project-1"),
        "审计范围要能定位到项目"
    );
    assert_eq!(
        event.decided_at, "2026-11-22T00:00:00Z",
        "业务时间取命令给的时刻，不从现在时刻反推"
    );
    assert_eq!(
        rows[0].recorded_at_ms, 1_000,
        "落库时间才是 now_ms——两者分开才有可恢复原值"
    );
}

/// 候选导入的审计依据是「凭什么」（只有定位），不是「产生了什么」。
#[test]
fn candidate_import_audits_the_candidate_basis() {
    let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
    let mut command = import_command("k-cand", "dt-c", "src-c");
    command.object_type = None;
    command.provider_object_id = None;
    command.locator = "https://acme.example/web/pull/318".to_string();
    with_uow(&mut store, |uow| {
        uow.import_delivery_source(&command, 1_000)
    })
    .expect("只有定位应进候选");

    let rows = store
        .list_audit_events("company-default", "req-import")
        .expect("应可读回");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].event.action, AuditAction::ImportSource);
    assert_eq!(rows[0].event.basis, "import-source:source_candidate");
}

/// 审计写不进去，整条导入回滚：不允许出现「业务成了、审计没有」的写入。
///
/// 构造方式是让业务字段全部合法、只有审计过不了闸门（`imported_by` 含凭据形态）。
/// 若审计被放在事务外，这个用例会看到一条落库的跟踪项——那正是要害。
#[test]
fn import_rolls_back_when_the_audit_row_is_refused() {
    let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
    let mut command = import_command("k-secret", "dt-x", "src-x");
    command.imported_by = "Authorization: Bearer abc123".to_string();

    let err = with_uow(&mut store, |uow| {
        uow.import_delivery_source(&command, 1_000)
    })
    .expect_err("审计拒收时导入必须失败");
    assert!(
        matches!(
            err,
            StoreError::Audit(crate::audit_log::AuditRejection::SecretInText("actor"))
        ),
        "{err:?}"
    );

    assert_eq!(
        count_rows(&store, "delivery_tracking"),
        0,
        "跟踪项必须一并回滚"
    );
    assert_eq!(
        count_rows(&store, "delivery_sources"),
        0,
        "来源身份必须一并回滚"
    );
    assert_eq!(
        count_rows(&store, "delivery_command_receipts"),
        0,
        "回执不得留下——否则重放会返回一个从未发生的成功"
    );
    assert_eq!(
        count_rows(&store, "audit_events"),
        0,
        "审计本身也不得留半条"
    );
}
