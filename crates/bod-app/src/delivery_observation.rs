//! P8 §11 观察登记（3.1 的写入口）：部署、回滚、人工补证。
//!
//! 本模块只有**规范化与校验**，没有 SQL：时间怎么归一、观察 ID 怎么派生、
//! 一条命令合法与否都是纯函数；落库在 `store::delivery::record_delivery_observation`。
//!
//! 四个判断决定了这条命令的形状，逐条写在这里而不是散在代码里：
//!
//! 1. **`observed_at` 必填，命令不默认「现在」**。观察的业务时间是**来源时间**，
//!    不是入库时间。默认「现在」会让晚到的补录（离线安装回执隔天才有）把一周前的
//!    部署记成今天，于是「回滚是否晚于部署」与时间线的乱序判定一起失效——而且不报错。
//!    代价是每条命令多一个参数；收益是「昨天部署、今天才录」仍然是昨天。
//! 2. **不推进跟踪项版本，也不要 `expected_version`**。追加事实不是「照着某个状态
//!    做的决定」：两个人同时各记一条部署，两条都该留下，中间没有可被覆盖的读-改-写
//!    窗口。这一点与 `link` 相反，反的不是风格而是语义——`link` 改的是关系集合，
//!    观察只往时间轴上追加。用例 `recording_an_observation_does_not_bump_the_version`
//!    与 `two_writers_record_at_the_same_time_without_conflict` 各钉一半。
//! 3. **`authorized` 不由命令行给出**。授权范围是 §34.12 的判定结果，写入口若提供
//!    `--authorized`，「授权」就变成被审者自己打的勾，与 ADR-0007「主体不得自报」
//!    同一种错。命令一律写 0：未授权的人工补证不会让覆盖变成「已覆盖」
//!    （判定侧给的是 `install-receipt-unauthorized`），这是安全的一侧，不是漏了。
//! 4. **观察不创建目标**。`link` 的 `--target` 语义是「**要确保存在**的目标」；
//!    观察的 `--target` 是「这条事实**发生在**哪个目标上」。后者不能凭一条可能打错
//!    客户名的观察凭空建出一条客户记录——那会把一次笔误变成一份对外承诺。
//!    目标不存在就报错并指向 `bod tracking link`。

use crate::delivery_api::TargetRef;
use crate::delivery_tracking::{ObservationKind, ObservationRow};

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

/// 观察登记的拒绝原因。码是稳定字符串：客户端与日志按码分支，改码等于改契约。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObservationError {
    /// 公司为空：观察没有公司，写进去就查不出来。
    EmptyCompanyId,
    EmptyTrackingId,
    /// 没有记录「谁登记的」：观察是责任归属，不能空。
    MissingRecorder,
    /// 目标身份字段为空/全空白。`field` 说明是哪一段。
    EmptyTargetField {
        field: &'static str,
        tracking_id: String,
    },
    /// 业务时间是空白：不是「用现在」，是没写。
    EmptyObservationTime,
    /// 业务时间不是合法的 RFC 3339。
    MalformedObservationTime(String),
    /// 声称来自连接器却给不出对应的来源修订。
    ///
    /// 库层原语不强制（3.1 落地时来源身份还没建表），入口层从这里开始强制：
    /// 不强制的话 `connector` 就成了一个谁都能写的词，「连接器观察」与
    /// 「人眼看到后自称连接器」在库里长得一模一样。
    ConnectorRequiresSourceRevision,
}

impl ObservationError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::EmptyCompanyId => "OBSERVATION_EMPTY_COMPANY",
            Self::EmptyTrackingId => "OBSERVATION_EMPTY_TRACKING",
            Self::MissingRecorder => "OBSERVATION_MISSING_RECORDER",
            Self::EmptyTargetField { .. } => "OBSERVATION_EMPTY_TARGET_FIELD",
            Self::EmptyObservationTime => "OBSERVATION_EMPTY_TIME",
            Self::MalformedObservationTime(_) => "OBSERVATION_MALFORMED_TIME",
            Self::ConnectorRequiresSourceRevision => "OBSERVATION_CONNECTOR_WITHOUT_SOURCE",
        }
    }

    pub fn explain(&self) -> String {
        match self {
            Self::EmptyCompanyId => "观察缺少公司：写进去也查不出来".to_string(),
            Self::EmptyTrackingId => "观察缺少跟踪项 ID".to_string(),
            Self::MissingRecorder => "登记观察必须记录操作者（actor 不能为空）".to_string(),
            Self::EmptyTargetField { field, tracking_id } => {
                format!("跟踪项 {tracking_id} 的目标缺少 {field}：目标身份不成立")
            }
            Self::EmptyObservationTime => {
                "缺少业务观察时间：晚到的补录不能记成今天，所以不默认取当前时刻".to_string()
            }
            Self::MalformedObservationTime(value) => format!(
                "业务观察时间 {value} 不是 RFC 3339（形如 2026-11-21T10:00:00Z 或 \
                 2026-11-21T18:00:00+08:00）"
            ),
            Self::ConnectorRequiresSourceRevision => {
                "来源类别是 connector 时必须给出来源修订：否则「连接器观察」只是一句自报。\
                 人工看到的事实请用 source_kind=manual"
                    .to_string()
            }
        }
    }
}

impl std::fmt::Display for ObservationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.explain())
    }
}

impl std::error::Error for ObservationError {}

// ---------------------------------------------------------------------------
// 业务时间
// ---------------------------------------------------------------------------

/// 把一条 `observed_at` 原文解析成（规范化 UTC 串, 毫秒）。
///
/// 为什么自己写：本仓不引时间库，而观察的业务时间要参与三件事——时间线排序、
/// 「回滚晚于部署」的判定、观察 ID 的派生。三件都要求同一时刻只有一种表示，
/// 所以必须解析后**重新格式化**，不能把原文直接存进去。
///
/// 接受的形态就是 RFC 3339 的常用子集：`YYYY-MM-DDTHH:MM:SS[.fff...](Z|±HH:MM)`。
/// 小数秒只保留毫秒（存储精度是毫秒），更细的位数被**截断**——这不是猜测，
/// 是本地存储精度不够；截断后的值仍然单调，不会颠倒两条观察的先后。
pub fn parse_observed_at(text: &str) -> Result<(String, i64), ObservationError> {
    let raw = text.trim();
    if raw.is_empty() {
        return Err(ObservationError::EmptyObservationTime);
    }
    let bad = || ObservationError::MalformedObservationTime(raw.to_string());
    let bytes = raw.as_bytes();
    if bytes.len() < 20 {
        return Err(bad());
    }
    if bytes[4] != b'-' || bytes[7] != b'-' || bytes[13] != b':' || bytes[16] != b':' {
        return Err(bad());
    }
    if !matches!(bytes[10], b'T' | b't') {
        return Err(bad());
    }
    let year = digits(&bytes[0..4]).ok_or_else(bad)?;
    let month = digits(&bytes[5..7]).ok_or_else(bad)? as u32;
    let day = digits(&bytes[8..10]).ok_or_else(bad)? as u32;
    let hour = digits(&bytes[11..13]).ok_or_else(bad)?;
    let minute = digits(&bytes[14..16]).ok_or_else(bad)?;
    let second = digits(&bytes[17..19]).ok_or_else(bad)?;
    if hour > 23 || minute > 59 || second > 59 {
        // 闰秒（60）也拒绝：本仓的存储与展示都不含闰秒，静默进位会把时刻算错一秒，
        // 而「差一秒」在「回滚是否晚于部署」上是能翻结论的。
        return Err(bad());
    }

    let mut cursor = 19;
    let mut millis = 0i64;
    if bytes.get(cursor) == Some(&b'.') {
        cursor += 1;
        let start = cursor;
        while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
            cursor += 1;
        }
        if cursor == start {
            return Err(bad());
        }
        let fraction = &raw[start..cursor];
        let take = fraction.len().min(3);
        let mut scaled = String::with_capacity(3);
        scaled.push_str(&fraction[..take]);
        while scaled.len() < 3 {
            scaled.push('0');
        }
        millis = scaled.parse().map_err(|_| bad())?;
    }

    let offset_secs = match bytes.get(cursor) {
        Some(b'Z') | Some(b'z') if cursor + 1 == bytes.len() => 0,
        Some(sign @ (b'+' | b'-')) => {
            // 只接受带冒号的 `±HH:MM`：`+0800` 在 RFC 3339 里不合法，宽收会让
            // 「解析成功」这件事的含义变模糊，而它决定的是时刻本身。
            let rest = &bytes[cursor + 1..];
            if rest.len() != 5 || rest[2] != b':' {
                return Err(bad());
            }
            let offset_hour = digits(&rest[0..2]).ok_or_else(bad)?;
            let offset_minute = digits(&rest[3..5]).ok_or_else(bad)?;
            if offset_hour > 23 || offset_minute > 59 {
                return Err(bad());
            }
            let magnitude = offset_hour * 3_600 + offset_minute * 60;
            if *sign == b'+' { magnitude } else { -magnitude }
        }
        _ => return Err(bad()),
    };

    let days = days_from_civil(year, month, day).ok_or_else(bad)?;
    let secs = days * 86_400 + hour * 3_600 + minute * 60 + second - offset_secs;
    let observed_at_ms = secs * 1_000 + millis;
    Ok((utc_stamp_with_millis(observed_at_ms), observed_at_ms))
}

fn digits(slice: &[u8]) -> Option<i64> {
    let mut value = 0i64;
    for byte in slice {
        if !byte.is_ascii_digit() {
            return None;
        }
        value = value * 10 + i64::from(byte - b'0');
    }
    Some(value)
}

/// 公历日期 → 距 1970-01-01 的天数（Howard Hinnant 的 days_from_civil）。
/// 校验月份与日号：`2026-02-30` 不是「解析失败得不明显」，它就是错的。
fn days_from_civil(year: i64, month: u32, day: u32) -> Option<i64> {
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let days_in_month = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    if day == 0 || day > days_in_month {
        return None;
    }
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (i64::from(month) + 9) % 12;
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

/// 毫秒时间戳 → UTC `YYYY-MM-DDTHH:MM:SS[.sss]Z`。
///
/// 秒的部分复用 `conformance_report::utc_stamp`（同一份公历换算只有一处实现）；
/// 只在小数位非零时补 `.sss`，让「同一时刻」只有一种写法——规范化不彻底的话，
/// 观察 ID 会因写法不同而分叉，重复上报就变成两条。
fn utc_stamp_with_millis(ms: i64) -> String {
    let base = crate::conformance_report::utc_stamp(ms);
    let millis = ms.rem_euclid(1_000);
    if millis == 0 {
        return base;
    }
    let head = base.strip_suffix('Z').unwrap_or(base.as_str());
    format!("{head}.{millis:03}Z")
}

// ---------------------------------------------------------------------------
// 命令
// ---------------------------------------------------------------------------

/// 观察登记的命令域。写进回执，与导入、关联分开比对——一个键换命令是冲突。
pub const COMMAND_RECORD_OBSERVATION: &str = "record-delivery-observation";

/// 来源类别的缺省推导：人工补证就是人工；部署与回滚缺省按连接器理解，
/// 但连接器**必须**带来源修订（见 `validate`），于是「人眼看到部署」会撞上这条校验，
/// 被迫显式声明 `manual`。这个摩擦是有意的：证据来源不该由机器替人猜。
pub fn default_source_kind(kind: ObservationKind) -> &'static str {
    match kind {
        ObservationKind::ManualEvidence => "manual",
        ObservationKind::Deployment | ObservationKind::Rollback => "connector",
    }
}

/// 一次「给某个目标登记一条观察」的请求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordObservationCommand {
    pub idempotency_key: String,
    pub company_id: String,
    pub tracking_id: String,
    /// 事实发生在哪个目标上。**不建目标**：见模块注释第 4 条。
    pub target: TargetRef,
    pub kind: ObservationKind,
    /// 已校验过的稳定取值：`connector` / `manual` / `internal`。
    pub source_kind: &'static str,
    /// 人类可读定位（如 `owner/repo#318`）。不参与判定，也不进观察 ID。
    pub source_ref: String,
    /// 关联的来源修订（3.2）。声称连接器观察时必须给。
    pub source_revision_id: Option<String>,
    pub artifact_revision: Option<String>,
    pub digest: String,
    /// 部署修订标识。同版本两个摘要才是冲突（`delivery_tracking::digest_conflict`），
    /// 所以这个字段虽然可以为空，空了就等于放弃「同一版本」这个判据。
    pub revision: String,
    /// 人工补证的授权范围与复验依据。空备注的补证由写入原语拒绝
    /// （`DeliveryError::EmptyManualEvidenceNote`）——这里不重复判一遍。
    pub note: String,
    pub actor_id: String,
    /// 业务时间（规范化后的 UTC 串）与它的毫秒值。两者由 `parse_observed_at` 同时产出。
    pub observed_at: String,
    pub observed_at_ms: i64,
    pub correlation_id: String,
    pub decided_at: String,
}

impl RecordObservationCommand {
    pub fn validate(&self) -> Result<(), ObservationError> {
        if self.company_id.trim().is_empty() {
            return Err(ObservationError::EmptyCompanyId);
        }
        if self.tracking_id.trim().is_empty() {
            return Err(ObservationError::EmptyTrackingId);
        }
        if self.actor_id.trim().is_empty() {
            return Err(ObservationError::MissingRecorder);
        }
        if self.target.customer_id.trim().is_empty() {
            return Err(ObservationError::EmptyTargetField {
                field: "customer_id",
                tracking_id: self.tracking_id.clone(),
            });
        }
        if self.target.environment.trim().is_empty() {
            return Err(ObservationError::EmptyTargetField {
                field: "environment",
                tracking_id: self.tracking_id.clone(),
            });
        }
        if matches!(self.target.deployment_unit.as_deref(), Some(unit) if unit.trim().is_empty()) {
            // 缺省与空串都是「整个环境」，全空白既不是环境也不是某个单元：拒绝，不猜。
            return Err(ObservationError::EmptyTargetField {
                field: "deployment_unit",
                tracking_id: self.tracking_id.clone(),
            });
        }
        if self.observed_at.trim().is_empty() {
            return Err(ObservationError::EmptyObservationTime);
        }
        if self.source_kind == "connector"
            && self
                .source_revision_id
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty()
        {
            return Err(ObservationError::ConnectorRequiresSourceRevision);
        }
        Ok(())
    }

    /// 目标身份的载荷表示：四段各自带长度前缀。
    ///
    /// 不拼成 `a:b:c`：客户 ID 或环境里出现分隔符时，两个不同的目标能拼出同一个串，
    /// 而载荷指纹正是用来区分「改了什么」的。
    fn target_identity_key(&self) -> String {
        let unit = self.target.deployment_unit.as_deref().unwrap_or("");
        crate::canonical_json::length_prefixed_parts(&[
            &self.target.customer_id,
            &self.target.environment,
            unit,
        ])
    }

    /// 观察 ID：`obs-` + 身份摘要。**派生而不是生成**：同一次事实重复上报必须落到同一行，
    /// 否则时间线会被轮询刷屏，而「同一重复事件只确认一次」就成了空话。
    ///
    /// 进 ID 的段是「发生了什么」：公司、跟踪项、目标、类别、来源类别、业务时间、
    /// 修订、摘要、制品修订、来源修订、备注。不进的段是「怎么记的」：
    /// - `actor_id`：谁先记下这次观察不是事实的一部分，是归属；同一事件由两人上报是一行。
    /// - `source_ref`：人类可读坐标，不参与判定，两种写法不该产生两条证据。
    /// - 幂等键与关联 ID：它们是命令的属性，归回执管（3.3）。
    ///
    /// `source_kind` **必须**进 ID：连接器与人工对同一时刻、同一版本可以各有一条，
    /// 而「两份证据冲突要保留两者」正是规格要求（§11）。合并成一条，冲突就消失了。
    pub fn observation_id(&self, target_id: &str) -> String {
        let ms = self.observed_at_ms.to_string();
        let digest = crate::canonical_json::length_prefixed_digest(&[
            &self.company_id,
            &self.tracking_id,
            target_id,
            self.kind.as_str(),
            self.source_kind,
            &ms,
            &self.revision,
            &self.digest,
            self.artifact_revision.as_deref().unwrap_or(""),
            self.source_revision_id.as_deref().unwrap_or(""),
            &self.note,
        ]);
        format!("obs-{digest}")
    }

    /// 载荷指纹的输入。顺序无关由 `payload_fingerprint` 的 `BTreeMap` 保证。
    ///
    /// 不含幂等键（含了等于换键即换载荷），不含关联 ID 与决策时刻（它们不是载荷）。
    /// 业务时间按**毫秒**入指纹而不按原文：`2026-11-21T10:00:00Z` 与
    /// `2026-11-21T18:00:00+08:00` 是同一时刻，判成两个载荷会让同一条命令重发时
    /// 报「同键异载荷」——那不是保护，是让人去改一个没错的地方。
    pub fn payload_fields(&self) -> Vec<(&'static str, String)> {
        vec![
            ("tracking_id", self.tracking_id.clone()),
            ("target", self.target_identity_key()),
            ("kind", self.kind.as_str().to_string()),
            ("source_kind", self.source_kind.to_string()),
            ("source_ref", self.source_ref.clone()),
            (
                "source_revision_id",
                self.source_revision_id.clone().unwrap_or_default(),
            ),
            (
                "artifact_revision",
                self.artifact_revision.clone().unwrap_or_default(),
            ),
            ("digest", self.digest.clone()),
            ("revision", self.revision.clone()),
            ("note", self.note.clone()),
            ("actor_id", self.actor_id.clone()),
            ("observed_at_ms", self.observed_at_ms.to_string()),
        ]
    }

    /// 落库行。`authorized` 恒为 `false`——见模块注释第 3 条。
    pub fn to_row(&self, target_id: &str, recorded_at_ms: i64) -> ObservationRow {
        ObservationRow {
            observation_id: self.observation_id(target_id),
            company_id: self.company_id.clone(),
            tracking_id: self.tracking_id.clone(),
            target_id: target_id.to_string(),
            kind: self.kind,
            source_kind: self.source_kind,
            source_ref: self.source_ref.clone(),
            source_revision_id: self.source_revision_id.clone(),
            artifact_revision: self.artifact_revision.clone(),
            digest: self.digest.clone(),
            revision: self.revision.clone(),
            note: self.note.clone(),
            actor_id: self.actor_id.clone(),
            authorized: false,
            observed_at: self.observed_at.clone(),
            observed_at_ms: self.observed_at_ms,
            recorded_at_ms,
        }
    }
}

// ---------------------------------------------------------------------------
// 结果
// ---------------------------------------------------------------------------

/// 登记结果。
///
/// `recorded=false` 表示这次事实已经在库里（同 ID 的行存在），本次没写；
/// `replayed=true` 是另一回事：同一条命令的重放（幂等键命中）。两者都可能为真。
///
/// `version` 是本次之后跟踪项的版本，**与提交前相同**——观察不推进版本（模块注释第 2 条）。
/// 把它放进结果是为了让「没推进」可核对，而不是让人以为漏了。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RecordObservationOutcome {
    pub tracking_id: String,
    pub target_id: String,
    pub observation_id: String,
    pub kind: String,
    pub source_kind: String,
    pub observed_at: String,
    pub recorded: bool,
    pub replayed: bool,
    /// 恒为 `false`。放在结果里而不是省略：调用方要能看见「这条补证还不算授权」。
    pub authorized: bool,
    pub version: u64,
}

impl RecordObservationOutcome {
    /// 从回执结果体还原一次重放。`replayed` 由调用方此刻的事实决定，不信存档里的值。
    pub fn replayed_from(detail: &str) -> Result<Self, String> {
        let mut outcome: Self =
            serde_json::from_str(detail).map_err(|err| format!("结果体不是合法 JSON：{err}"))?;
        outcome.replayed = true;
        Ok(outcome)
    }
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

    fn command() -> RecordObservationCommand {
        let (observed_at, observed_at_ms) =
            parse_observed_at("2026-11-21T10:00:00Z").expect("时间应可解析");
        RecordObservationCommand {
            idempotency_key: "k-1".to_string(),
            company_id: "company-default".to_string(),
            tracking_id: "dt-1".to_string(),
            target: target("acme", "prod", Some("web")),
            kind: ObservationKind::Deployment,
            source_kind: "connector",
            source_ref: "acme/web#318".to_string(),
            source_revision_id: Some("rev-1".to_string()),
            artifact_revision: Some("artifact-1".to_string()),
            digest: "sha256:abc".to_string(),
            revision: "deploy-2026-11-21".to_string(),
            note: String::new(),
            actor_id: "alice".to_string(),
            observed_at,
            observed_at_ms,
            correlation_id: "req-1".to_string(),
            decided_at: "2026-11-21T10:01:00Z".to_string(),
        }
    }

    /// 同一时刻两种写法必须归一到同一个值：ID 与指纹都靠它。
    #[test]
    fn rfc3339_offsets_collapse_to_the_same_instant() {
        let utc = parse_observed_at("2026-11-21T10:00:00Z").expect("应可解析");
        let offset = parse_observed_at("2026-11-21T18:00:00+08:00").expect("应可解析");
        assert_eq!(utc, offset);
        assert_eq!(utc.0, "2026-11-21T10:00:00Z");
        // 毫秒保留，且小数位非零时带 `.sss`。
        let millis = parse_observed_at("2026-11-21T10:00:00.250Z").expect("应可解析");
        assert_eq!(millis.0, "2026-11-21T10:00:00.250Z");
        assert_eq!(millis.1, utc.1 + 250);
    }

    /// 时间解析不是宽容的：错日期、错范围、缺偏移都要报错，别把错值算成一个时刻。
    #[test]
    fn malformed_times_are_refused_not_guessed() {
        for bad in [
            "2026-02-30T00:00:00Z",
            "2026-13-01T00:00:00Z",
            "2026-11-21T24:00:00Z",
            "2026-11-21T10:60:00Z",
            "2026-11-21T10:00:60Z",
            "2026-11-21T10:00:00",
            "2026-11-21T10:00:00+0800",
            "2026-11-21 10:00:00Z",
            "2026-11-21T10:00:00.Z",
            "not-a-time",
        ] {
            assert!(
                matches!(
                    parse_observed_at(bad),
                    Err(ObservationError::MalformedObservationTime(_))
                ),
                "{bad} 应被拒绝"
            );
        }
        assert_eq!(
            parse_observed_at("   "),
            Err(ObservationError::EmptyObservationTime)
        );
        // 闰年 2 月 29 日是合法的，非闰年不是。
        assert!(parse_observed_at("2024-02-29T00:00:00Z").is_ok());
        assert!(parse_observed_at("2026-02-29T00:00:00Z").is_err());
    }

    /// 观察 ID 不含「谁记的」与人类可读坐标：同一事件两种记法仍是一行。
    #[test]
    fn observation_id_ignores_recorder_and_human_locator() {
        let base = command();
        let mut other = base.clone();
        other.actor_id = "bob".to_string();
        other.source_ref = "另一处写法".to_string();
        assert_eq!(
            base.observation_id("dtg-1"),
            other.observation_id("dtg-1"),
            "记录者与定位不是「发生了什么」"
        );

        // 来源类别必须进 ID：连接器与人工各有一条正是「保留两者」。
        let mut manual = base.clone();
        manual.source_kind = "manual";
        manual.source_revision_id = None;
        assert_ne!(base.observation_id("dtg-1"), manual.observation_id("dtg-1"));

        // 业务时间进 ID：晚到补录的旧事件不能与今天的同版本部署合并。
        let mut earlier = base.clone();
        earlier.observed_at_ms -= 1;
        assert_ne!(
            base.observation_id("dtg-1"),
            earlier.observation_id("dtg-1")
        );
    }

    /// 声称连接器却无来源修订：拒绝，并指向两条正当出路。
    #[test]
    fn connector_claims_require_a_source_revision() {
        let mut cmd = command();
        cmd.source_revision_id = None;
        let err = cmd.validate().expect_err("应拒绝");
        assert_eq!(err.code(), "OBSERVATION_CONNECTOR_WITHOUT_SOURCE");
        assert!(err.explain().contains("manual"), "{err}");

        cmd.source_kind = "manual";
        assert!(cmd.validate().is_ok(), "人工观察不需要来源修订");
    }

    /// 缺省来源类别：人工补证是人工，其余按连接器——于是必须带来源修订。
    #[test]
    fn default_source_kind_depends_on_the_kind() {
        assert_eq!(
            default_source_kind(ObservationKind::ManualEvidence),
            "manual"
        );
        assert_eq!(
            default_source_kind(ObservationKind::Deployment),
            "connector"
        );
        assert_eq!(default_source_kind(ObservationKind::Rollback), "connector");
    }

    /// 载荷指纹：时间按毫秒比，不按原文。
    #[test]
    fn payload_treats_two_spellings_of_one_instant_as_equal() {
        let base = command();
        let mut other = command();
        let (text, ms) = parse_observed_at("2026-11-21T18:00:00+08:00").expect("应可解析");
        other.observed_at = text;
        other.observed_at_ms = ms;
        assert_eq!(base.payload_fields(), other.payload_fields());

        let mut changed = command();
        changed.digest = "sha256:def".to_string();
        assert_ne!(base.payload_fields(), changed.payload_fields());
    }

    /// 落库行的 `authorized` 不由命令决定。
    #[test]
    fn rows_are_written_unauthorized_regardless_of_the_command() {
        let row = command().to_row("dtg-1", 1_000);
        assert!(!row.authorized);
        assert_eq!(row.observation_id, command().observation_id("dtg-1"));
    }
}
