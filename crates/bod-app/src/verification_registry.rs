//! G-50 验证索引：I/G 绑定、状态与覆盖校验（任务 8.3）。
//!
//! `verification/gates.yaml` 编译进二进制（`include_str!`），因此校验结果与构建产物一致，
//! 不依赖运行时文件或网络。
//!
//! 本模块只做**可静态判定**的检查：
//! 1. 结构：id 唯一、格式合法、字段齐全、status 合法；
//! 2. 证据口径：`implemented` 必须给出 module 与 tests，`planned` 不得填 tests；
//! 3. 覆盖：Change 内 spec/proposal 提到的每个 I/G 都必须有条目；
//! 4. 反向：登记了但规格从未引用的条目单独列出（不判失败，避免误杀前瞻绑定）。
//!
//! "测试路径是否真实存在"需要读工作区源码，由 `xtask verify --stage P0` 完成。

use std::collections::{BTreeMap, BTreeSet};

pub const REGISTRY_PATH: &str = "verification/gates.yaml";

/// 已编译进二进制的索引正文。
pub fn shipped_registry() -> &'static str {
    include_str!("../../../verification/gates.yaml")
}

/// 绑定意图来源（名称 → 正文），用于引用覆盖检查。
///
/// 只取 `proposal.md` 与 `tasks.md`：9 份 spec delta 只描述行为，不含 I/G 编号
/// （实测 9 份现行规格里 `I-`/`G-` 命中数均为 0，唯一命中在 proposal），
/// 把 spec 正文当来源会得到"零引用"的假覆盖。
///
/// 路径指向归档目录：P0 于 2026-11-19 归档（`openspec archive`），Change 正文移到
/// `openspec/changes/archive/2026-11-19-p0-minimal-trusted-core/`。归档是单一来源，
/// 不复制副本；若再次归档导致目录名变化，这里必须同步，否则编译期即失败（不会静默漏检）。
///
/// P0 归档来源保留，避免 P1 出口切换时丢掉历史约定的覆盖检查；P1 出口（任务 5.3）
/// 追加当前 Change 的 proposal/tasks，使 P1 引用的 I-71 与 G-15/G-16/G-51 等也进入
/// `coverage_gaps()`。P1 归档后必须把这四条路径同步到 archive 目录，编译期即失败，
/// 不会静默漏检。
pub const BINDING_SOURCES: &[(&str, &str)] = &[
    (
        "p0-proposal",
        include_str!(
            "../../../openspec/changes/archive/2026-11-19-p0-minimal-trusted-core/proposal.md"
        ),
    ),
    (
        "p0-tasks",
        include_str!(
            "../../../openspec/changes/archive/2026-11-19-p0-minimal-trusted-core/tasks.md"
        ),
    ),
    (
        "p1-proposal",
        include_str!("../../../openspec/changes/p1-single-executor-delivery/proposal.md"),
    ),
    (
        "p1-tasks",
        include_str!("../../../openspec/changes/p1-single-executor-delivery/tasks.md"),
    ),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingKind {
    Invariant,
    Gate,
}

impl BindingKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Invariant => "invariant",
            Self::Gate => "gate",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "invariant" => Some(Self::Invariant),
            "gate" => Some(Self::Gate),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindingStatus {
    Implemented,
    Planned,
    Unverified,
}

impl BindingStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Implemented => "implemented",
            Self::Planned => "planned",
            Self::Unverified => "unverified",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "implemented" => Some(Self::Implemented),
            "planned" => Some(Self::Planned),
            "unverified" => Some(Self::Unverified),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub id: String,
    pub kind: BindingKind,
    pub requirement: String,
    pub module: String,
    pub tests: String,
    pub status: BindingStatus,
    pub evidence: String,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Registry {
    pub bindings: Vec<Binding>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct RegistryError {
    pub line: usize,
    pub message: String,
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "第 {} 行：{}", self.line, self.message)
    }
}

impl std::error::Error for RegistryError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryProblem {
    DuplicateId(String),
    BadId(String),
    MissingField { id: String, field: &'static str },
    ImplementedWithoutEvidence(String),
    PlannedWithTests(String),
    UnknownKind(String),
}

impl std::fmt::Display for RegistryProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateId(id) => write!(f, "{id} 重复登记"),
            Self::BadId(id) => write!(f, "{id} 不是合法 I-NN/G-N 形式"),
            Self::MissingField { id, field } => write!(f, "{id} 缺少字段 {field}"),
            Self::ImplementedWithoutEvidence(id) => {
                write!(f, "{id} 标为 implemented 但没有 module/tests")
            }
            Self::PlannedWithTests(id) => {
                write!(f, "{id} 标为 planned 却填了 tests，等于把计划当证据")
            }
            Self::UnknownKind(kind) => write!(f, "未知 kind：{kind}"),
        }
    }
}

/// 严格解析最小 YAML 子集：`- id:` 起一条记录，其后为缩进的 `key: value`。
///
/// 只接受本文件实际使用的键；未知键、重复键、缺少 `- id` 一律报错，
/// 不静默忽略（避免索引写错却显示通过）。
pub fn parse(text: &str) -> Result<Registry, RegistryError> {
    let mut bindings: Vec<Binding> = Vec::new();
    let mut current: Option<(usize, BTreeMap<String, String>)> = None;

    for (index, raw) in text.lines().enumerate() {
        let line_no = index + 1;
        let line = raw.trim_end();
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("- ") {
            if let Some((start, fields)) = current.take() {
                bindings.push(build(start, fields)?);
            }
            let mut fields = BTreeMap::new();
            let (key, value) = split_pair(rest, line_no)?;
            fields.insert(key, value);
            current = Some((line_no, fields));
            continue;
        }
        let Some((start, fields)) = current.as_mut() else {
            return Err(RegistryError {
                line: line_no,
                message: "记录外的键值行".to_string(),
            });
        };
        let _ = start;
        let (key, value) = split_pair(trimmed, line_no)?;
        if fields.insert(key.clone(), value).is_some() {
            return Err(RegistryError {
                line: line_no,
                message: format!("重复键 {key}"),
            });
        }
    }
    if let Some((start, fields)) = current.take() {
        bindings.push(build(start, fields)?);
    }
    Ok(Registry { bindings })
}

fn split_pair(text: &str, line: usize) -> Result<(String, String), RegistryError> {
    let Some((key, value)) = text.split_once(':') else {
        return Err(RegistryError {
            line,
            message: format!("缺少冒号：{text}"),
        });
    };
    let key = key.trim().to_string();
    let value = value.trim().trim_matches('"').to_string();
    if key.is_empty() {
        return Err(RegistryError {
            line,
            message: "空键".to_string(),
        });
    }
    const ALLOWED: &[&str] = &[
        "id",
        "kind",
        "requirement",
        "module",
        "tests",
        "status",
        "evidence",
        "note",
    ];
    if !ALLOWED.contains(&key.as_str()) {
        return Err(RegistryError {
            line,
            message: format!("未知键 {key}"),
        });
    }
    Ok((key, value))
}

fn build(line: usize, fields: BTreeMap<String, String>) -> Result<Binding, RegistryError> {
    let take = |key: &str| fields.get(key).cloned().unwrap_or_default();
    let id = take("id");
    if id.is_empty() {
        return Err(RegistryError {
            line,
            message: "记录缺少 id".to_string(),
        });
    }
    let kind_raw = take("kind");
    let kind = BindingKind::parse(&kind_raw).ok_or_else(|| RegistryError {
        line,
        message: format!("未知 kind：{kind_raw}"),
    })?;
    let status_raw = take("status");
    let status = BindingStatus::parse(&status_raw).ok_or_else(|| RegistryError {
        line,
        message: format!("未知 status：{status_raw}"),
    })?;
    Ok(Binding {
        id,
        kind,
        requirement: take("requirement"),
        module: take("module"),
        tests: take("tests"),
        status,
        evidence: take("evidence"),
        note: take("note"),
    })
}

/// 结构校验：不读源码，只判断索引自身是否自洽。
pub fn validate(registry: &Registry) -> Vec<RegistryProblem> {
    let mut problems = Vec::new();
    let mut seen = BTreeSet::new();
    for binding in &registry.bindings {
        if !is_valid_id(&binding.id) {
            problems.push(RegistryProblem::BadId(binding.id.clone()));
        }
        if !seen.insert(binding.id.clone()) {
            problems.push(RegistryProblem::DuplicateId(binding.id.clone()));
        }
        if binding.requirement.trim().is_empty() {
            problems.push(RegistryProblem::MissingField {
                id: binding.id.clone(),
                field: "requirement",
            });
        }
        match binding.status {
            BindingStatus::Implemented => {
                if binding.module.trim().is_empty() || binding.tests.trim().is_empty() {
                    problems.push(RegistryProblem::ImplementedWithoutEvidence(
                        binding.id.clone(),
                    ));
                }
            }
            BindingStatus::Planned => {
                if !binding.tests.trim().is_empty() {
                    problems.push(RegistryProblem::PlannedWithTests(binding.id.clone()));
                }
            }
            BindingStatus::Unverified => {}
        }
    }
    problems
}

fn is_valid_id(id: &str) -> bool {
    if let Some(rest) = id.strip_prefix("I-") {
        return rest.len() == 2 && rest.chars().all(|c| c.is_ascii_digit());
    }
    if let Some(rest) = id.strip_prefix("G-") {
        return !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit());
    }
    false
}

/// 从规格正文里提取 I/G 引用，规范化为 `I-01` / `G-20`。
///
/// 识别三种写法：`invariant_i_01`、`I-01`、`G-20`。
pub fn references_in(text: &str) -> BTreeSet<String> {
    let bytes = text.as_bytes();
    let mut found = BTreeSet::new();
    let mut index = 0;
    while index < bytes.len() {
        // 只在 ASCII 位置上切片：多字节字符中间的偏移不是 char 边界。
        if !bytes[index].is_ascii() {
            index += 1;
            continue;
        }
        if text[index..].starts_with("invariant_i_") {
            let start = index + "invariant_i_".len();
            if let Some(end) = digits_end(bytes, start) {
                found.insert(format!("I-{:0>2}", &text[start..end]));
                index = end;
                continue;
            }
        }
        let is_boundary = index == 0 || !is_word_byte(bytes[index - 1]);
        if is_boundary {
            for prefix in ["I-", "G-"] {
                if text[index..].starts_with(prefix) {
                    let start = index + prefix.len();
                    if let Some(end) = digits_end(bytes, start) {
                        let width = if prefix == "I-" { 2 } else { 1 };
                        let number = &text[start..end];
                        let normalized = if number.len() < width {
                            format!("{}{:0>width$}", prefix, number)
                        } else {
                            format!("{prefix}{number}")
                        };
                        found.insert(normalized);
                        index = end;
                        break;
                    }
                }
            }
        }
        index += 1;
    }
    found
}

fn digits_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut end = start;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    if end == start { None } else { Some(end) }
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// 绑定意图里出现、但索引没有登记的 I/G。
pub fn coverage_gaps(registry: &Registry) -> Vec<String> {
    let known: BTreeSet<&str> = registry
        .bindings
        .iter()
        .map(|binding| binding.id.as_str())
        .collect();
    let mut gaps = BTreeSet::new();
    for (_, text) in BINDING_SOURCES {
        for reference in references_in(text) {
            if !known.contains(reference.as_str()) {
                gaps.insert(reference);
            }
        }
    }
    gaps.into_iter().collect()
}

/// 索引里登记、但 proposal/tasks 从未引用的条目（纠正性绑定，不判失败）。
pub fn unreferenced_bindings(registry: &Registry) -> Vec<String> {
    let mut referenced = BTreeSet::new();
    for (_, text) in BINDING_SOURCES {
        referenced.extend(references_in(text));
    }
    registry
        .bindings
        .iter()
        .filter(|binding| !referenced.contains(&binding.id))
        .map(|binding| binding.id.clone())
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct G50Report {
    pub total: usize,
    pub implemented: usize,
    pub planned: usize,
    pub unverified: usize,
    pub gaps: Vec<String>,
    pub unreferenced: Vec<String>,
    pub problems: Vec<RegistryProblem>,
}

impl G50Report {
    /// 全量覆盖 + 结构自洽才算通过；未引用条目只做提示。
    pub fn ok(&self) -> bool {
        self.problems.is_empty() && self.gaps.is_empty()
    }

    pub fn summary(&self) -> String {
        format!(
            "共 {} 条绑定：implemented {} / planned {} / unverified {}；缺失引用 {}；结构问题 {}",
            self.total,
            self.implemented,
            self.planned,
            self.unverified,
            self.gaps.len(),
            self.problems.len()
        )
    }
}

pub fn g50_report() -> Result<G50Report, RegistryError> {
    let registry = parse(shipped_registry())?;
    let problems = validate(&registry);
    Ok(G50Report {
        total: registry.bindings.len(),
        implemented: registry
            .bindings
            .iter()
            .filter(|binding| binding.status == BindingStatus::Implemented)
            .count(),
        planned: registry
            .bindings
            .iter()
            .filter(|binding| binding.status == BindingStatus::Planned)
            .count(),
        unverified: registry
            .bindings
            .iter()
            .filter(|binding| binding.status == BindingStatus::Unverified)
            .count(),
        gaps: coverage_gaps(&registry),
        unreferenced: unreferenced_bindings(&registry),
        problems,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_registry_parses_and_is_self_consistent() {
        let registry = parse(shipped_registry()).expect("索引必须可解析");
        assert!(!registry.bindings.is_empty());
        let problems = validate(&registry);
        assert!(problems.is_empty(), "结构问题：{problems:?}");
    }

    #[test]
    fn registry_covers_every_spec_reference() {
        let report = g50_report().expect("索引必须可解析");
        assert!(
            report.gaps.is_empty(),
            "以下 I/G 在绑定意图里被引用但索引未登记：{:?}",
            report.gaps
        );
        assert!(report.ok(), "G-50 未通过：{}", report.summary());
    }

    #[test]
    fn duplicate_id_is_rejected() {
        let text = "\
- id: I-01
  kind: invariant
  requirement: a
  module: m
  tests: t
  status: implemented
- id: I-01
  kind: invariant
  requirement: b
  module: m
  tests: t
  status: implemented
";
        let registry = parse(text).unwrap();
        assert!(
            validate(&registry)
                .iter()
                .any(|problem| matches!(problem, RegistryProblem::DuplicateId(id) if id == "I-01"))
        );
    }

    #[test]
    fn planned_with_tests_is_rejected() {
        let text = "\
- id: I-10
  kind: invariant
  requirement: a
  module: m
  tests: some::test
  status: planned
";
        let registry = parse(text).unwrap();
        assert!(
            validate(&registry)
                .iter()
                .any(|problem| matches!(problem, RegistryProblem::PlannedWithTests(_)))
        );
    }

    #[test]
    fn unknown_key_is_a_parse_error() {
        let text = "\
- id: I-01
  knd: invariant
";
        // 未知键必须报错，而不是被忽略。
        let error = parse(text).unwrap_err();
        assert!(error.message.contains("kind") || error.message.contains("未知键"));
    }

    #[test]
    fn reference_extraction_normalizes_forms() {
        let text = "见 invariant_i_07 与 I-26、G-2，还有 I-9 与 I-52。";
        let found = references_in(text);
        assert!(found.contains("I-07"));
        assert!(found.contains("I-26"));
        assert!(found.contains("G-2"));
        assert!(found.contains("I-09"), "I-9 应规范化为 I-09");
        assert!(found.contains("I-52"));
        assert_eq!(found.len(), 5);
    }

    /// 8.4 的机器可检查部分：支持矩阵里每个 Supported 行都必须给出证据，不能是占位横线。
    #[test]
    fn support_matrix_supported_rows_carry_evidence() {
        let doc = include_str!("../../../docs/spec-notes/p0-support-matrix.md");
        let mut checked = 0usize;
        for line in doc.lines() {
            let cells: Vec<&str> = line.split('|').map(str::trim).collect();
            // 表格行：["", 能力, 状态, 证据, 说明, ""]
            if cells.len() < 5 || !cells[2].starts_with("Supported") {
                continue;
            }
            checked += 1;
            let evidence = cells[3];
            assert!(
                !evidence.is_empty() && evidence != "—" && evidence != "-",
                "Supported 行缺少证据：{line}"
            );
        }
        assert!(
            checked >= 10,
            "支持矩阵应至少包含 10 个 Supported 行，实际 {checked}"
        );
    }

    #[test]
    fn missing_reference_is_reported_as_gap() {
        let registry = parse(
            "\
- id: I-01
  kind: invariant
  requirement: a
  module: m
  tests: t
  status: implemented
",
        )
        .unwrap();
        let gaps = coverage_gaps(&registry);
        assert!(!gaps.is_empty(), "规格中的其它 I/G 应被报为缺口");
        assert!(!gaps.contains(&"I-01".to_string()));
    }
}
