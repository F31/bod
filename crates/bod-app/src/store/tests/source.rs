//! 来源身份、候选与修订（P8 3.2）：五元组去重、修订按事件幂等、候选确认是终局。

use super::*;

// -----------------------------------------------------------------------

fn source_identity(project: &str, provider_id: &str) -> crate::delivery_source::SourceIdentity {
    crate::delivery_source::SourceIdentity {
        company_id: "company-default".to_string(),
        project_id: project.to_string(),
        connection_id: "gh-1".to_string(),
        object_type: "pull-request".to_string(),
        provider_object_id: provider_id.to_string(),
    }
}

fn source_proposal(locator: &str) -> crate::delivery_source::CandidateProposal {
    crate::delivery_source::CandidateProposal {
        company_id: "company-default".to_string(),
        project_id: "project-1".to_string(),
        connection_id: "gh-1".to_string(),
        // 只有 URL 时对象类型往往还没解析出来：空串表示「未定」，不猜。
        object_type: String::new(),
        locator: locator.to_string(),
    }
}

fn source_revision(
    id: &str,
    source_id: &str,
    version: &str,
    digest: &str,
    at_ms: i64,
) -> crate::delivery_source::SourceRevisionRow {
    let fingerprint =
        crate::delivery_source::revision_fingerprint(version, digest).expect("指纹应可生成");
    crate::delivery_source::SourceRevisionRow {
        source_revision_id: id.to_string(),
        company_id: "company-default".to_string(),
        source_id: source_id.to_string(),
        version: version.to_string(),
        digest: digest.to_string(),
        content_fingerprint: fingerprint,
        actor_id: "ci".to_string(),
        observed_at: format!("2026-11-21T10:00:{:02}Z", at_ms % 60),
        observed_at_ms: at_ms,
        recorded_at_ms: at_ms,
    }
}

/// 五元组相同 → 只登记一次；首个确认人不被后来的调用覆盖。
#[test]
fn same_source_identity_is_registered_once() {
    let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
    let identity = source_identity("project-1", "PR_kw1");

    let first = with_uow(&mut store, |uow| {
        uow.register_source("src-1", &identity, "acme/web#318", "alice", 100)
    })
    .expect("首次登记应成功");
    assert!(first.created);
    assert_eq!(first.source_id, "src-1");

    let again = with_uow(&mut store, |uow| {
        uow.register_source("src-2", &identity, "acme/web#318", "bob", 200)
    })
    .expect("重复登记应返回已有身份");
    assert!(!again.created, "重复登记不得新建");
    assert_eq!(again.source_id, "src-1");

    let row = store
        .get_source("company-default", "src-1")
        .expect("读取应成功")
        .expect("来源应存在");
    assert_eq!(row.confirmed_by, "alice", "首个确认人是历史事实，不被覆盖");
    assert_eq!(row.confirmed_at_ms, 100);
    assert_eq!(
        store
            .list_sources("company-default", None, 10)
            .expect("列表应可读")
            .len(),
        1
    );
}

/// 跨项目同名 PR：五元组里 project 不同就是两个来源，不会因编号相同而合并。
#[test]
fn same_number_in_two_projects_stay_two_sources() {
    let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
    with_uow(&mut store, |uow| {
        uow.register_source(
            "src-1",
            &source_identity("project-1", "PR_kw318"),
            "acme/web#318",
            "alice",
            100,
        )
        .expect("项目 1 的来源应可登记");
        uow.register_source(
            "src-2",
            &source_identity("project-2", "PR_kw318"),
            "acme/api#318",
            "alice",
            100,
        )
        .expect("项目 2 的来源应可登记");
    });

    assert_eq!(
        store
            .list_sources("company-default", Some("project-1"), 10)
            .expect("列表应可读")
            .len(),
        1
    );
    assert_eq!(
        store
            .list_sources("company-default", None, 10)
            .expect("列表应可读")
            .len(),
        2
    );
}

/// 同一事件重复上报只确认一次；同一标签两个摘要留两条修订。
#[test]
fn source_revision_is_idempotent_per_event() {
    let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
    with_uow(&mut store, |uow| {
        uow.register_source(
            "src-1",
            &source_identity("project-1", "PR_kw1"),
            "acme/web#318",
            "alice",
            100,
        )
        .expect("来源应可登记");
    });

    let first = with_uow(&mut store, |uow| {
        uow.append_source_revision(&source_revision(
            "rev-1",
            "src-1",
            "v1.4.2",
            "sha256:aaa",
            100,
        ))
    })
    .expect("首次修订应可写入");
    assert!(first.created);

    // 同一 (version, digest) 隔一分钟再报：仍是同一条修订。
    let duplicate = with_uow(&mut store, |uow| {
        uow.append_source_revision(&source_revision(
            "rev-2",
            "src-1",
            "v1.4.2",
            "sha256:aaa",
            160,
        ))
    })
    .expect("重复事件应返回已有修订");
    assert!(!duplicate.created, "同一事件不得新增修订");
    assert_eq!(duplicate.source_revision_id, "rev-1");

    // 可变标签：同一版本换了摘要 → 新的观察，必须留痕。
    let changed = with_uow(&mut store, |uow| {
        uow.append_source_revision(&source_revision(
            "rev-3",
            "src-1",
            "v1.4.2",
            "sha256:bbb",
            220,
        ))
    })
    .expect("不同摘要应可追加");
    assert!(changed.created);

    let revisions = store
        .list_source_revisions("company-default", "src-1")
        .expect("时间线应可读");
    assert_eq!(revisions.len(), 2);
    assert_eq!(revisions[0].source_revision_id, "rev-1", "按观察时间升序");
    assert_eq!(revisions[1].digest, "sha256:bbb");
    assert_eq!(
        crate::delivery_source::digest_history(&revisions).len(),
        2,
        "两次解析都要在时间线里"
    );
}

/// 指纹必须由内容派生：调用方自己编的指纹会让幂等键失效。
#[test]
fn revision_fingerprint_must_match_content() {
    let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
    with_uow(&mut store, |uow| {
        uow.register_source(
            "src-1",
            &source_identity("project-1", "PR_kw1"),
            "acme/web#318",
            "alice",
            100,
        )
        .expect("来源应可登记");
    });
    let mut revision = source_revision("rev-1", "src-1", "v1", "sha256:aaa", 100);
    revision.content_fingerprint = "forged".to_string();
    let err = with_uow(&mut store, |uow| uow.append_source_revision(&revision))
        .expect_err("伪造指纹必须被拒绝");
    assert!(
        matches!(
            err,
            StoreError::Source(crate::delivery_source::SourceError::FingerprintMismatch { .. })
        ),
        "{err:?}"
    );
}

/// 修订是历史：库里拒绝改写，回滚与新解析都只能追加。
#[test]
fn source_revisions_are_append_only() {
    let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
    with_uow(&mut store, |uow| {
        uow.register_source(
            "src-1",
            &source_identity("project-1", "PR_kw1"),
            "acme/web#318",
            "alice",
            100,
        )
        .expect("来源应可登记");
        uow.append_source_revision(&source_revision("rev-1", "src-1", "v1", "sha256:aaa", 100))
            .expect("修订应可写入");
    });
    let update = store.conn.execute(
            "UPDATE delivery_source_revisions SET digest = 'sha256:bbb' WHERE source_revision_id = 'rev-1'",
            [],
        );
    assert!(update.is_err(), "改写修订必须被触发器拒绝");
    let delete = store.conn.execute(
        "DELETE FROM delivery_source_revisions WHERE source_revision_id = 'rev-1'",
        [],
    );
    assert!(delete.is_err(), "删除修订必须被触发器拒绝");
}

/// 候选确认：一次动作两个写入，且只能发生一次。
#[test]
fn candidate_confirmation_creates_identity_and_is_final() {
    let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
    let proposed = with_uow(&mut store, |uow| {
        uow.propose_source_candidate(
            "cand-1",
            &source_proposal("https://github.com/acme/web/pull/318"),
            "alice",
            100,
        )
    })
    .expect("候选应可提交");
    assert!(proposed.created);

    // 同一 URL 再提一次：还是一条待办，不是第二条。
    let duplicate = with_uow(&mut store, |uow| {
        uow.propose_source_candidate(
            "cand-2",
            &source_proposal("https://github.com/acme/web/pull/318"),
            "bob",
            120,
        )
    })
    .expect("重复候选应返回已有待办");
    assert!(!duplicate.created);
    assert_eq!(duplicate.candidate_id, "cand-1");

    let confirmed = with_uow(&mut store, |uow| {
        uow.confirm_source_candidate(
            "company-default",
            "cand-1",
            "src-1",
            &source_identity("project-1", "PR_kw1"),
            "alice",
            200,
        )
    })
    .expect("确认应成功");
    assert!(confirmed.source_created);
    assert_eq!(confirmed.source_id, "src-1");

    let candidates = store
        .list_source_candidates("company-default", None, false, 10)
        .expect("候选列表应可读");
    assert_eq!(candidates.len(), 1);
    assert_eq!(
        candidates[0].state,
        crate::delivery_source::CandidateState::Confirmed
    );
    assert_eq!(candidates[0].resolved_source_id.as_deref(), Some("src-1"));
    assert_eq!(candidates[0].decided_by, "alice");

    // 再确认一次：拒绝，且不得留下第二个身份。
    let err = with_uow(&mut store, |uow| {
        uow.confirm_source_candidate(
            "company-default",
            "cand-1",
            "src-9",
            &source_identity("project-1", "PR_kw2"),
            "bob",
            300,
        )
    })
    .expect_err("已裁决的候选不得再确认");
    assert!(
        matches!(
            err,
            StoreError::Source(crate::delivery_source::SourceError::CandidateAlreadyDecided(_))
        ),
        "{err:?}"
    );
    assert_eq!(
        store
            .list_sources("company-default", None, 10)
            .expect("列表应可读")
            .len(),
        1,
        "失败的确认不得留下身份"
    );
}

/// 确认不得把候选挪到别的项目：候选的作用域就是它被提交时的项目。
#[test]
fn candidate_scope_cannot_move_to_another_project() {
    let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
    with_uow(&mut store, |uow| {
        uow.propose_source_candidate(
            "cand-1",
            &source_proposal("https://github.com/acme/web/pull/318"),
            "alice",
            100,
        )
        .expect("候选应可提交");
    });
    let err = with_uow(&mut store, |uow| {
        uow.confirm_source_candidate(
            "company-default",
            "cand-1",
            "src-1",
            &source_identity("project-2", "PR_kw1"),
            "alice",
            200,
        )
    })
    .expect_err("跨项目确认必须拒绝");
    assert!(
        matches!(
            err,
            StoreError::Source(crate::delivery_source::SourceError::CandidateScopeMismatch { .. })
        ),
        "{err:?}"
    );
    assert!(
        store
            .list_sources("company-default", None, 10)
            .expect("列表应可读")
            .is_empty()
    );
}

/// 观察挂来源修订：同项目才允许，且证据引用改用来源修订 ID。
#[test]
fn observation_links_same_project_source_revision() {
    let mut store = SqliteStore::in_memory(&config()).expect("内存库应可打开");
    seed_delivery(&mut store);
    with_uow(&mut store, |uow| {
        uow.register_source(
            "src-1",
            &source_identity("project-1", "PR_kw1"),
            "acme/web#318",
            "alice",
            100,
        )
        .expect("来源应可登记");
        uow.append_source_revision(&source_revision("rev-1", "src-1", "v1", "sha256:abc", 100))
            .expect("修订应可写入");
    });

    let mut linked = delivery_observation(
        "o-9",
        "t-1",
        ObservationKind::Deployment,
        300,
        "rev-1",
        "sha256:abc",
        "",
    );
    linked.source_revision_id = Some("rev-1".to_string());
    with_uow(&mut store, |uow| {
        uow.append_delivery_observation(&linked)
            .expect("同项目来源的观察应可写入");
    });

    let view = store
        .delivery_tracking_view("company-default", "dt-1")
        .expect("读取应成功")
        .expect("跟踪项应存在");
    let web = view
        .targets
        .iter()
        .find(|target| target.target.target_id == "t-1")
        .expect("web 目标应存在");
    assert_eq!(
        web.evidence_revision.as_deref(),
        Some("rev-1"),
        "证据引用应改用来源修订 ID"
    );

    // 另一个项目的来源：同一公司也不行，跨项目来源不得挂到本项目交付上。
    with_uow(&mut store, |uow| {
        uow.register_source(
            "src-2",
            &source_identity("project-2", "PR_kw9"),
            "acme/api#9",
            "alice",
            100,
        )
        .expect("另一个项目的来源应可登记");
        uow.append_source_revision(&source_revision("rev-2", "src-2", "v1", "sha256:abc", 100))
            .expect("修订应可写入");
    });
    let mut cross = delivery_observation(
        "o-10",
        "t-1",
        ObservationKind::Deployment,
        400,
        "rev-1",
        "sha256:abc",
        "",
    );
    cross.source_revision_id = Some("rev-2".to_string());
    let err = with_uow(&mut store, |uow| uow.append_delivery_observation(&cross))
        .expect_err("跨项目来源必须拒绝");
    assert!(
        matches!(
            err,
            StoreError::Source(crate::delivery_source::SourceError::SourceProjectMismatch { .. })
        ),
        "{err:?}"
    );

    // 不存在的修订：给出可读原因，而不是外键报错。
    let mut missing = delivery_observation(
        "o-11",
        "t-1",
        ObservationKind::Deployment,
        500,
        "rev-1",
        "sha256:abc",
        "",
    );
    missing.source_revision_id = Some("rev-404".to_string());
    let err = with_uow(&mut store, |uow| uow.append_delivery_observation(&missing))
        .expect_err("不存在的修订必须拒绝");
    assert!(
        matches!(
            err,
            StoreError::Source(crate::delivery_source::SourceError::SourceRevisionNotFound(
                _
            ))
        ),
        "{err:?}"
    );
}
