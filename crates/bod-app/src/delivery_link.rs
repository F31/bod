//! P8 §11 关联追加（3.4）：目标关联、`expected_version` 与「共享来源不合并客户记录」。
//!
//! 本模块只有**规范化与校验**，没有 SQL：目标怎么归一、关系 ID 怎么派生、
//! 一条命令合法与否，都是纯函数；落库在 `store::append_delivery_link`。
//!
//! 四条不能让步的语义，测试逐条钉住：
//!
//! 1. **追加要带期望版本，且只有真的追加成功才推进版本**。纯重复（关系已在、目标已在）
//!    不 +1：否则两个客户端会互相把对方的 `expected_version` 判过期，谁都不敢重试——
//!    「重试不重复建项」就变成了「重试必冲突」。
//! 2. **一个来源身份只承载一条跟踪关系**（018 的主键）。所以「一个 PR 支撑两个客户的交付」
//!    在模型里是**一条跟踪项 + 两个目标**，不是两条跟踪项。客户记录按目标各自独立：
//!    客户 A 的部署观察不得改变客户 B 的结论，也不得出现在它的证据里。
//! 3. **目标身份不 trim、不猜**。`"prod"` 与 `"prod "` 是两个目标，静默合并等于把两条
//!    客户记录压成一条；反过来 `None` 与 `Some("")` 是同一个目标——存储层用空串表示
//!    「整个环境」（SQLite 唯一索引里 NULL 互不相等），这是同一个东西的两种写法。
//! 4. **关系 ID 由身份派生，不落库**。关系的身份就是（公司, 种类, 跟踪项, 引用）四段；
//!    派生而不是拼接，是为了让四段里的分隔符无法伪造出同一个 ID。
//!
//! 刻意不做的事：本模块不判断「这条关联是否足够支撑覆盖结论」。关联是事实，
//! 结论仍只由 `coverage_receipt::judge_coverage` 在读时算出（design D1）。

use crate::delivery_api::{DeliveryLinkResponse, TargetRef};
use crate::domain::artifact::Digest;

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

/// 关联追加的拒绝原因。码是稳定字符串：客户端与日志按码分支，改码等于改契约。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkError {
    /// 公司为空：关联没有公司，写进去就查不出来。
    EmptyCompanyId,
    EmptyTrackingId,
    /// 没有记录「谁追加的」：关联是责任归属，不能空。
    MissingLinker,
    /// `source` 与 `work_package_revision` 都没给：这条命令不知道该挂什么。
    LinkReferenceMissing,
    /// 两个都给了：契约要求恰好一个。**不替调用方挑**——挑错了就是挂错对象，
    /// 而挂错的关联会一直躺在记录里影响后续判定。
    LinkReferenceAmbiguous,
    /// 引用是空白：不是「挂空」，是没写。
    EmptyLinkReference,
    /// 目标字段为空/全空白：`stage` 说明是哪一段。
    EmptyTargetField {
        field: &'static str,
        tracking_id: String,
    },
    /// 跟踪项已关闭：关闭表示交付承诺已结清，不再追加关联。
    /// 与视图同口径——`allowed_actions` 在关闭时本来就不给 `link-source`，
    /// 写入口若还放行，视图与事实就分家了。
    TrackingClosed(String),
    /// 期望版本与当前版本不一致。带上当前版本，客户端不必再读一次。
    VersionConflict {
        expected: u64,
        current: u64,
    },
}

impl LinkError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::EmptyCompanyId => "LINK_EMPTY_COMPANY",
            Self::EmptyTrackingId => "LINK_EMPTY_TRACKING",
            Self::MissingLinker => "LINK_MISSING_LINKER",
            Self::LinkReferenceMissing => "LINK_REFERENCE_MISSING",
            Self::LinkReferenceAmbiguous => "LINK_REFERENCE_AMBIGUOUS",
            Self::EmptyLinkReference => "LINK_REFERENCE_EMPTY",
            Self::EmptyTargetField { .. } => "LINK_EMPTY_TARGET_FIELD",
            Self::TrackingClosed(_) => "LINK_TRACKING_CLOSED",
            Self::VersionConflict { .. } => "LINK_VERSION_CONFLICT",
        }
    }

    pub fn explain(&self) -> String {
        match self {
            Self::EmptyCompanyId => "关联缺少公司：写进去也查不出来".to_string(),
            Self::EmptyTrackingId => "关联缺少跟踪项 ID".to_string(),
            Self::MissingLinker => "追加关联必须记录操作者（linked_by 不能为空）".to_string(),
            Self::LinkReferenceMissing => {
                "source 与 work_package_revision 必须给一个：这条命令不知道要挂什么".to_string()
            }
            Self::LinkReferenceAmbiguous => {
                "source 与 work_package_revision 只能给一个：两者同时给时由谁决定挂哪个都是猜"
                    .to_string()
            }
            Self::EmptyLinkReference => "关联引用是空白：不是「挂空」，是没写".to_string(),
            Self::EmptyTargetField { field, tracking_id } => {
                format!("跟踪项 {tracking_id} 的目标缺少 {field}：目标身份不成立")
            }
            Self::TrackingClosed(id) => {
                format!("跟踪项 {id} 已关闭：关闭后不再追加关联（视图也不再提供该动作）")
            }
            Self::VersionConflict { expected, current } => format!(
                "期望版本 {expected} 与当前版本 {current} 不一致：\
                 说明期间有别的写入，请按当前版本重新提交"
            ),
        }
    }
}

impl std::fmt::Display for LinkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.explain())
    }
}

impl std::error::Error for LinkError {}

// ---------------------------------------------------------------------------
// 关联的两种引用
// ---------------------------------------------------------------------------

/// 追加的是哪一种东西。两种引用**不能同时存在**（见 `from_options`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkRef {
    /// 已确认的来源身份（`delivery_sources.source_id`）。
    Source(String),
    /// 内部工作包修订。只追加关联，不建 Attempt（§11）。
    WorkPackage(String),
}

impl LinkRef {
    /// 契约里的两个可选字段 → 一个引用。恰好给一个才成立。
    ///
    /// 不给报 `LinkReferenceMissing`，给两个报 `LinkReferenceAmbiguous`：
    /// 两者的修法不同（补字段 vs 删字段），合成一档会让调用方试错。
    pub fn from_options(
        source: Option<String>,
        work_package_revision: Option<String>,
    ) -> Result<Self, LinkError> {
        let reference = match (source, work_package_revision) {
            (Some(source), None) => Self::Source(source),
            (None, Some(revision)) => Self::WorkPackage(revision),
            (None, None) => return Err(LinkError::LinkReferenceMissing),
            (Some(_), Some(_)) => return Err(LinkError::LinkReferenceAmbiguous),
        };
        if reference.value().trim().is_empty() {
            return Err(LinkError::EmptyLinkReference);
        }
        Ok(reference)
    }

    /// 关系种类。写进关系 ID，也是回执里区分「挂的是来源还是工作包」的字段。
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Source(_) => "source",
            Self::WorkPackage(_) => "work-package",
        }
    }

    /// 引用本身（来源 ID 或工作包修订号）。**不 trim**：标识符按原样查，
    /// 多一个空格就该查不到，而不是悄悄按另一个标识符处理。
    pub fn value(&self) -> &str {
        match self {
            Self::Source(value) | Self::WorkPackage(value) => value,
        }
    }
}

// ---------------------------------------------------------------------------
// 目标归一
// ---------------------------------------------------------------------------

/// 归一后的目标：身份三段定下来，ID 由身份派生。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedTarget {
    pub target_id: String,
    pub customer_id: String,
    pub environment: String,
    /// 空串表示整个环境。
    pub deployment_unit: String,
}

impl NormalizedTarget {
    pub fn to_ref(&self) -> TargetRef {
        TargetRef {
            customer_id: self.customer_id.clone(),
            environment: self.environment.clone(),
            deployment_unit: if self.deployment_unit.is_empty() {
                None
            } else {
                Some(self.deployment_unit.clone())
            },
        }
    }
}

/// 目标 ID：`dtg-` + 四段的摘要。派生而不是计数，重试才能落到同一行。
///
/// 四段按长度前缀编码再求摘要：`("a","bc")` 与 `("ab","c")` 不能拼出同一个 ID。
pub fn target_id(company_id: &str, tracking_id: &str, target: &TargetRef) -> String {
    target_id_of(
        company_id,
        tracking_id,
        &target.customer_id,
        &target.environment,
        target.deployment_unit.as_deref().unwrap_or(""),
    )
}

/// 归一后的目标直接按四段算 ID，不为了复用签名再造一个 `TargetRef`。
fn target_id_of(
    company_id: &str,
    tracking_id: &str,
    customer_id: &str,
    environment: &str,
    deployment_unit: &str,
) -> String {
    let digest = digest_of_parts(&[
        company_id,
        tracking_id,
        customer_id,
        environment,
        deployment_unit,
    ]);
    format!("dtg-{digest}")
}

/// 关系 ID：`dl-` + 四段的摘要。
///
/// 不含目标与版本：关系说的是「谁挂在谁身上」，不是「哪一次请求挂的」。
/// 因此同一关系无论追加几次、追加到哪些目标上，ID 都是同一个。
pub fn link_id(company_id: &str, kind: &str, tracking_id: &str, reference: &str) -> String {
    let digest = digest_of_parts(&[company_id, kind, tracking_id, reference]);
    format!("dl-{digest}")
}

/// 长度前缀拼接后求 sha256，返回 64 位 hex（算法前缀由调用方决定要不要）。
fn digest_of_parts(parts: &[&str]) -> String {
    let mut material = String::new();
    for part in parts {
        material.push_str(&part.chars().count().to_string());
        material.push(':');
        material.push_str(part);
    }
    Digest::of_bytes(material.as_bytes()).value
}

// ---------------------------------------------------------------------------
// 命令
// ---------------------------------------------------------------------------

/// 关联追加的命令域。写进回执，与导入命令分开比对——一个键换命令是冲突。
pub const COMMAND_APPEND_LINK: &str = "append-delivery-link";

/// 一次「给跟踪项追加关联」的请求。
///
/// `targets` 是**要确保存在的目标**：已有的按身份命中原行、不动它的验收条件与开关
/// （那是事实，不能被一次关联悄悄改写），没有的才新建。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppendLinkCommand {
    pub idempotency_key: String,
    pub company_id: String,
    pub tracking_id: String,
    pub link: LinkRef,
    pub targets: Vec<TargetRef>,
    /// 期望的跟踪项版本。不一致就拒绝，不做「尽力而为」的合并。
    pub expected_version: u64,
    /// 谁追加的。责任归属，也进载荷指纹。
    pub linked_by: String,
    /// 这次动作的关联 ID（入口层给）。与 `ImportSourceCommand::correlation_id` 同款：
    /// 不拿幂等键顶替，也不进载荷指纹。
    pub correlation_id: String,
    /// 这次追加的业务时刻（RFC 3339），直接作为审计的 `decided_at`。
    /// 与 `ImportSourceCommand::decided_at` 同款：不进载荷指纹，也不从 `now_ms` 反推。
    pub decided_at: String,
}

impl AppendLinkCommand {
    /// 校验除目标以外的字段。
    pub fn validate(&self) -> Result<(), LinkError> {
        if self.company_id.trim().is_empty() {
            return Err(LinkError::EmptyCompanyId);
        }
        if self.tracking_id.trim().is_empty() {
            return Err(LinkError::EmptyTrackingId);
        }
        if self.linked_by.trim().is_empty() {
            return Err(LinkError::MissingLinker);
        }
        if self.link.value().trim().is_empty() {
            return Err(LinkError::EmptyLinkReference);
        }
        Ok(())
    }

    /// 把目标归一：校验身份三段、补上派生 ID、按 ID 去重（重复给同一目标是一条指令，不是两条）。
    ///
    /// **保持调用顺序**：写库顺序影响 `created_at_ms`，也让人读日志时看到的顺序与请求一致。
    pub fn normalized_targets(&self) -> Result<Vec<NormalizedTarget>, LinkError> {
        let mut seen = Vec::with_capacity(self.targets.len());
        for target in &self.targets {
            if target.customer_id.trim().is_empty() {
                return Err(LinkError::EmptyTargetField {
                    field: "customer_id",
                    tracking_id: self.tracking_id.clone(),
                });
            }
            if target.environment.trim().is_empty() {
                return Err(LinkError::EmptyTargetField {
                    field: "environment",
                    tracking_id: self.tracking_id.clone(),
                });
            }
            let deployment_unit = match target.deployment_unit.as_deref() {
                // 缺省与空串都是「整个环境」：存储层只有一种写法，别在入口留两种。
                None | Some("") => String::new(),
                // 全空白既不是「整个环境」也不是某个单元：拒绝，不猜。
                Some(unit) if unit.trim().is_empty() => {
                    return Err(LinkError::EmptyTargetField {
                        field: "deployment_unit",
                        tracking_id: self.tracking_id.clone(),
                    });
                }
                Some(unit) => unit.to_string(),
            };
            let normalized = NormalizedTarget {
                target_id: target_id_of(
                    &self.company_id,
                    &self.tracking_id,
                    &target.customer_id,
                    &target.environment,
                    &deployment_unit,
                ),
                customer_id: target.customer_id.clone(),
                environment: target.environment.clone(),
                deployment_unit,
            };
            if !seen
                .iter()
                .any(|existing: &NormalizedTarget| existing.target_id == normalized.target_id)
            {
                seen.push(normalized);
            }
        }
        Ok(seen)
    }

    /// 这条命令建立的关系的 ID。
    pub fn link_id(&self) -> String {
        link_id(
            &self.company_id,
            self.link.kind(),
            &self.tracking_id,
            self.link.value(),
        )
    }

    /// 载荷指纹的输入。顺序无关由 `payload_fingerprint` 的 `BTreeMap` 保证；
    /// 目标列表先按 ID 排序再拼，让「换个顺序重试」仍算同一条命令。
    ///
    /// 不含 `idempotency_key`（同 3.3：含了就等于换键即换载荷）。
    /// 含 `expected_version`：「改掉期望版本再重试」是对另一个状态的另一条命令。
    pub fn payload_fields(&self, targets: &[NormalizedTarget]) -> Vec<(&'static str, String)> {
        let mut ids: Vec<&str> = targets
            .iter()
            .map(|target| target.target_id.as_str())
            .collect();
        ids.sort_unstable();
        vec![
            ("tracking_id", self.tracking_id.clone()),
            ("kind", self.link.kind().to_string()),
            ("reference", self.link.value().to_string()),
            ("targets", ids.join(",")),
            ("expected_version", self.expected_version.to_string()),
            ("linked_by", self.linked_by.clone()),
        ]
    }
}

// ---------------------------------------------------------------------------
// 结果与事实行
// ---------------------------------------------------------------------------

/// 追加结果。`appended=false` 表示关系与目标都已存在，本次没写，版本也没动。
///
/// `replayed=true` 是另一回事：那是**同一条命令**的重放（幂等键命中）。
/// 两个标志都可能为真是刻意的——重放一次本来就什么都没写。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LinkOutcome {
    pub tracking_id: String,
    pub link_id: String,
    pub appended: bool,
    /// 本次之后跟踪项的版本。`appended=false` 时等于提交时的期望版本。
    pub version: u64,
    /// 本次新建的目标 ID。放在回执里回答「这条命令当时建了什么」。
    #[serde(default)]
    pub created_targets: Vec<String>,
    pub replayed: bool,
}

impl LinkOutcome {
    /// 从回执结果体还原一次重放。`replayed` 由调用方此刻的事实决定，不信存档里的值。
    pub fn replayed_from(detail: &str) -> Result<Self, String> {
        let mut outcome: Self =
            serde_json::from_str(detail).map_err(|err| format!("结果体不是合法 JSON：{err}"))?;
        outcome.replayed = true;
        Ok(outcome)
    }

    /// 投影成冻结契约里的响应体（`POST /api/v1/delivery-tracking/{id}/links`）。
    ///
    /// `replayed` 不进契约：那一刻的事实已经体现在 `appended` 与 `version` 上，
    /// 而契约里多一个字段就要改 2.2 冻结的响应形状。
    pub fn to_dto(&self) -> DeliveryLinkResponse {
        DeliveryLinkResponse {
            tracking_id: self.tracking_id.clone(),
            link_id: self.link_id.clone(),
            appended: self.appended,
            version: self.version,
        }
    }
}

/// `delivery_tracking_work_packages` 一行。只读不写。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkPackageLink {
    pub tracking_id: String,
    pub work_package_revision: String,
    pub linked_by: String,
    pub linked_at_ms: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(customer: &str, environment: &str, unit: Option<&str>) -> TargetRef {
        TargetRef {
            customer_id: customer.to_string(),
            environment: environment.to_string(),
            deployment_unit: unit.map(str::to_string),
        }
    }

    fn command(targets: Vec<TargetRef>) -> AppendLinkCommand {
        AppendLinkCommand {
            idempotency_key: "k-1".to_string(),
            company_id: "company-default".to_string(),
            tracking_id: "dt-1".to_string(),
            link: LinkRef::Source("src-1".to_string()),
            targets,
            expected_version: 3,
            linked_by: "alice".to_string(),
            correlation_id: "req-1".to_string(),
            decided_at: "2026-11-22T00:00:00Z".to_string(),
        }
    }

    /// 恰好给一个引用：不给与给两个是两种错误，修法不同。
    #[test]
    fn link_reference_must_be_exactly_one() {
        assert_eq!(
            LinkRef::from_options(None, None),
            Err(LinkError::LinkReferenceMissing)
        );
        assert_eq!(
            LinkRef::from_options(Some("src-1".into()), Some("wp-7".into())),
            Err(LinkError::LinkReferenceAmbiguous)
        );
        assert_eq!(
            LinkRef::from_options(Some("  ".into()), None),
            Err(LinkError::EmptyLinkReference)
        );
        assert_eq!(
            LinkRef::from_options(Some("src-1".into()), None),
            Ok(LinkRef::Source("src-1".to_string()))
        );
        assert_eq!(
            LinkRef::from_options(None, Some("wp-7".into())),
            Ok(LinkRef::WorkPackage("wp-7".to_string()))
        );
    }

    /// 关系 ID 由身份派生：同一关系多次追加是同一个 ID；任一段变了就是另一个。
    #[test]
    fn link_id_is_derived_from_identity_only() {
        let first = link_id("c1", "source", "dt-1", "src-1");
        let again = link_id("c1", "source", "dt-1", "src-1");
        assert_eq!(first, again);
        assert!(first.starts_with("dl-"), "{first}");
        assert_eq!(first.len(), "dl-".len() + 64);
        // 换种类、换跟踪项、换引用、换公司，都必须是另一个 ID。
        assert_ne!(first, link_id("c1", "work-package", "dt-1", "src-1"));
        assert_ne!(first, link_id("c1", "source", "dt-2", "src-1"));
        assert_ne!(first, link_id("c1", "source", "dt-1", "src-2"));
        assert_ne!(first, link_id("c2", "source", "dt-1", "src-1"));
    }

    /// 目标 ID 与关系 ID 都不能被分段拼接伪造。
    #[test]
    fn derived_ids_cannot_be_forged_by_concatenation() {
        let a = target_id("c1", "dt-1", &target("acme", "bc", None));
        let b = target_id("c1", "dt-1", &target("acm", "ebc", None));
        assert_ne!(a, b);
        let c = link_id("c1", "source", "dt-1", "ab");
        let d = link_id("c1", "source", "dt-1a", "b");
        assert_ne!(c, d);
    }

    /// 同一目标给两遍是一条指令：去重后只剩一个，顺序保持调用顺序。
    #[test]
    fn duplicate_targets_collapse_but_order_is_kept() {
        let command = command(vec![
            target("acme", "prod", Some("web")),
            target("beta", "prod", None),
            target("acme", "prod", Some("web")),
        ]);
        let normalized = command.normalized_targets().expect("应可归一");
        assert_eq!(normalized.len(), 2);
        assert_eq!(normalized[0].customer_id, "acme");
        assert_eq!(normalized[1].customer_id, "beta");
    }

    /// `None` 与 `Some("")` 是同一个目标（整个环境）；全空白不是「整个环境」，拒绝。
    #[test]
    fn absent_and_empty_deployment_unit_are_the_same_target() {
        let command = command(vec![
            target("acme", "prod", None),
            target("acme", "prod", Some("")),
        ]);
        let normalized = command.normalized_targets().expect("应可归一");
        assert_eq!(normalized.len(), 1, "两种写法不能变成两条客户记录");
        assert!(normalized[0].deployment_unit.is_empty());
        // 回投影时也只给出一种写法：`Some("")` 不该从这里漏出去。
        assert_eq!(normalized[0].to_ref(), target("acme", "prod", None));

        let blank = command_for(target("acme", "prod", Some("   ")));
        assert_eq!(
            blank.normalized_targets(),
            Err(LinkError::EmptyTargetField {
                field: "deployment_unit",
                tracking_id: "dt-1".to_string(),
            })
        );
    }

    fn command_for(target: TargetRef) -> AppendLinkCommand {
        command(vec![target])
    }

    /// 目标身份不 trim：`"prod "` 与 `"prod"` 是两个客户记录，不静默合并。
    #[test]
    fn target_identity_is_not_trimmed() {
        let spaced = command(vec![target("acme", "prod ", None)]);
        let plain = command(vec![target("acme", "prod", None)]);
        assert_ne!(
            spaced.normalized_targets().unwrap()[0].target_id,
            plain.normalized_targets().unwrap()[0].target_id,
            "多一个空格是另一条记录"
        );
    }

    /// 缺客户或环境：目标身份不成立，报出是哪一段。
    #[test]
    fn empty_target_fields_are_rejected() {
        assert_eq!(
            command(vec![target("  ", "prod", None)]).normalized_targets(),
            Err(LinkError::EmptyTargetField {
                field: "customer_id",
                tracking_id: "dt-1".to_string(),
            })
        );
        assert_eq!(
            command(vec![target("acme", "", None)]).normalized_targets(),
            Err(LinkError::EmptyTargetField {
                field: "environment",
                tracking_id: "dt-1".to_string(),
            })
        );
    }

    /// 载荷指纹：换目标顺序不算换载荷，换目标集合算；不含幂等键。
    #[test]
    fn payload_ignores_target_order_but_not_target_set() {
        let a = command(vec![
            target("acme", "prod", None),
            target("beta", "prod", None),
        ]);
        let b = command(vec![
            target("beta", "prod", None),
            target("acme", "prod", None),
        ]);
        let fingerprint = |command: &AppendLinkCommand| {
            let targets = command.normalized_targets().unwrap();
            let owned = command.payload_fields(&targets);
            let borrowed: Vec<(&str, &str)> = owned
                .iter()
                .map(|(name, value)| (*name, value.as_str()))
                .collect();
            crate::delivery_idempotency::payload_fingerprint(&borrowed).unwrap()
        };
        assert_eq!(fingerprint(&a), fingerprint(&b), "换顺序不是换命令");

        let mut other = a.clone();
        other.idempotency_key = "k-2".to_string();
        assert_eq!(fingerprint(&a), fingerprint(&other), "换键不改载荷");

        let narrower = command(vec![target("acme", "prod", None)]);
        assert_ne!(
            fingerprint(&a),
            fingerprint(&narrower),
            "少一个目标是另一条命令"
        );

        let mut bumped = a.clone();
        bumped.expected_version = 4;
        assert_ne!(
            fingerprint(&a),
            fingerprint(&bumped),
            "改期望版本是另一条命令"
        );
    }

    /// 结果体可往返；重放标志由调用方此刻决定。
    #[test]
    fn outcome_round_trips_and_replay_flag_is_recomputed() {
        let outcome = LinkOutcome {
            tracking_id: "dt-1".to_string(),
            link_id: "dl-x".to_string(),
            appended: true,
            version: 4,
            created_targets: vec!["dtg-1".to_string()],
            replayed: false,
        };
        let text = serde_json::to_string(&outcome).expect("应可序列化");
        let restored = LinkOutcome::replayed_from(&text).expect("应可反序列化");
        assert_eq!(restored.tracking_id, "dt-1");
        assert_eq!(restored.created_targets, vec!["dtg-1".to_string()]);
        assert!(restored.appended && restored.replayed);
        assert!(LinkOutcome::replayed_from("not json").is_err());
    }

    /// 契约投影只出四个字段：多一个字段就要改 2.2 冻结的响应形状。
    #[test]
    fn outcome_projects_onto_the_frozen_contract() {
        let outcome = LinkOutcome {
            tracking_id: "dt-1".to_string(),
            link_id: "dl-x".to_string(),
            appended: false,
            version: 3,
            created_targets: Vec::new(),
            replayed: true,
        };
        let rendered = serde_json::to_value(outcome.to_dto()).expect("应可序列化");
        let mut keys: Vec<&str> = rendered
            .as_object()
            .expect("应是对象")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["appended", "link_id", "tracking_id", "version"]);
        assert_eq!(rendered["version"], serde_json::json!(3));
    }

    /// 校验只挑字段问题：公司、跟踪项、操作者、引用都不能空。
    #[test]
    fn command_validation_names_the_missing_field() {
        let mut empty_company = command(Vec::new());
        empty_company.company_id = "  ".to_string();
        assert_eq!(empty_company.validate(), Err(LinkError::EmptyCompanyId));

        let mut empty_tracking = command(Vec::new());
        empty_tracking.tracking_id = String::new();
        assert_eq!(empty_tracking.validate(), Err(LinkError::EmptyTrackingId));

        let mut no_actor = command(Vec::new());
        no_actor.linked_by = " ".to_string();
        assert_eq!(no_actor.validate(), Err(LinkError::MissingLinker));

        let mut blank_ref = command(Vec::new());
        blank_ref.link = LinkRef::WorkPackage("  ".to_string());
        assert_eq!(blank_ref.validate(), Err(LinkError::EmptyLinkReference));

        assert_eq!(command(Vec::new()).validate(), Ok(()));
    }
}
