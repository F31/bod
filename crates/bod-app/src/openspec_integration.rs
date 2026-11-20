//! P3 任务 1.1—1.6：OpenSpec 规格、任务与工作包之间的确定映射（`openspec-integration`）。
//!
//! 这一层不碰执行、不写库、不调度，只回答四个问题：
//! 1. 一个 Change 目录里的规格，此刻的**确定修订**是哪一个（不是"磁盘上最新"）；
//! 2. tasks.md 的每一行对应哪个**稳定 ID**（重排不变、改语义要人确认）；
//! 3. 一份拆分计划是否合格（覆盖、循环、共享冲突、大小、是否机械均分）；
//! 4. 勾选能不能当验收证据（不能），以及归档能不能并行（同 requirement 必须串行）。
//!
//! ## 为什么不用行号或显示编号做任务身份
//!
//! tasks.md 的 `1.2` 是**给人看的**编号，插入一条就会整体挪位；行号同理。任何按这两者绑定
//! 的做法，都会在下一次编辑时把"任务 3 的完成状态"错挂到"任务 4"头上。本模块只认
//! **规范化正文的指纹**：正文一字不改，顺序怎么变都还是同一个 ID；正文改了，就不许自动
//! 认亲，必须进 `Unresolved` 让人确认。
//!
//! ## 修订语义
//!
//! [`SpecRevision::digest`] 是规格文件规范化后的摘要。工作包引用它，而不是引用路径：
//! 文件后来被改了，工作包记录的 `spec_commit`/`digest` 仍然是旧的，[`SpecRevision::matches_file`]
//! 会报不一致——这就是「后续文件改动不静默改变其输入」（I-10）。

use crate::domain::artifact::sha256_hex;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::fs;
use std::path::Path;

// ---------------------------------------------------------------------------
// 错误
// ---------------------------------------------------------------------------

/// 解析与校验失败。全部为稳定分类，便于上层给出可执行的提示。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenspecError {
    Io(String),
    /// 规格文件里没有任何 `### Requirement:`。
    NoRequirements(String),
    /// 同一规格内两个 requirement 标题规范化后相同，引用会歧义。
    DuplicateRequirement {
        spec: String,
        title: String,
    },
    /// requirement 出现 `#### Scenario:` 之前没有任何 requirement。
    OrphanScenario {
        spec: String,
        title: String,
    },
    /// Change 目录缺少 `specs/`、`tasks.md` 等必要组成。
    IncompleteChange {
        path: String,
        missing: String,
    },
    /// 归档前置条件不满足（集成验收未过 / 一致性检查未跑或失败）。
    NotReadyToArchive {
        change_id: String,
        why: String,
    },
    /// 同一 requirement 被多个 Change 修改，必须串行，不能并行写入现行规格。
    RequiresSerialArchive {
        requirement: String,
        changes: Vec<String>,
    },
}

impl fmt::Display for OpenspecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(message) => write!(f, "{message}"),
            Self::NoRequirements(path) => {
                write!(f, "{path} 不含任何 `### Requirement:`，无法建立规格修订")
            }
            Self::DuplicateRequirement { spec, title } => write!(
                f,
                "{spec} 内 requirement 标题重复：`{title}`；标题是引用键，不能重复"
            ),
            Self::OrphanScenario { spec, title } => write!(
                f,
                "{spec} 的 scenario `{title}` 出现在任何 requirement 之前"
            ),
            Self::IncompleteChange { path, missing } => {
                write!(f, "Change 目录 {path} 缺少 {missing}")
            }
            Self::NotReadyToArchive { change_id, why } => {
                write!(f, "Change {change_id} 还不能归档：{why}")
            }
            Self::RequiresSerialArchive {
                requirement,
                changes,
            } => write!(
                f,
                "requirement `{requirement}` 被 {} 同时修改（{}），必须串行归档",
                changes.len(),
                changes.join(", ")
            ),
        }
    }
}

impl std::error::Error for OpenspecError {}

// ---------------------------------------------------------------------------
// 规格修订、需求与场景引用
// ---------------------------------------------------------------------------

/// 某个规格文件在某一刻的确定修订。
///
/// `digest` 覆盖规范化后的整份文本（去掉行尾空白与连续空行），因此"只改了一个空格"
/// 不会造成假漂移，而"改了一个字"一定漂移。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecRevision {
    pub spec_path: String,
    pub digest: String,
    pub requirement_count: usize,
    pub scenario_count: usize,
    pub captured_at_ms: i64,
}

impl SpecRevision {
    /// 与给定文本是否仍是同一修订。工作包派发前用它确认输入没被悄悄改过。
    pub fn matches_file(&self, text: &str) -> bool {
        self.digest == spec_digest(text)
    }

    /// 以 `sha256:` 前缀给出，与仓库其他摘要口径一致。
    pub fn digest_ref(&self) -> String {
        format!("sha256:{}", self.digest)
    }
}

/// 一条 `### Requirement:` 的稳定引用。键是**标题**（规范化成 slug），不是行号。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequirementRef {
    pub spec_path: String,
    /// 由标题得到的稳定键，如 `变更与规格修订映射`。
    pub id: String,
    pub title: String,
    /// 该 requirement 正文（含其 scenario）的摘要，用于判断"同一条需求被改写了"。
    pub text_digest: String,
}

/// 一条 `#### Scenario:` 的稳定引用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScenarioRef {
    pub spec_path: String,
    pub requirement_id: String,
    pub title: String,
    /// 同一条 requirement 内出现同名 scenario 时用序号区分；恒从 0 开始，与行号无关。
    pub ordinal: usize,
    pub text_digest: String,
}

/// 一份规格文件的解析结果：修订 + 需求 + 场景。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpecSnapshot {
    pub revision: SpecRevision,
    pub requirements: Vec<RequirementRef>,
    pub scenarios: Vec<ScenarioRef>,
}

impl SpecSnapshot {
    /// 解析 `specs/<capability>/spec.md`。
    ///
    /// 只认两级标题：`### Requirement: <title>` 与 `#### Scenario: <title>`。其余内容
    /// （Purpose、注释）计入所在块的正文摘要，参与漂移判定但不产生引用。
    pub fn parse(spec_path: &str, text: &str, captured_at_ms: i64) -> Result<Self, OpenspecError> {
        struct ScenarioBlock {
            title: String,
            body: String,
        }
        struct RequirementBlock {
            title: String,
            body: String,
            scenarios: Vec<ScenarioBlock>,
        }

        let normalized = normalize_text(text);
        let mut blocks: Vec<RequirementBlock> = Vec::new();
        let mut in_requirement = false;
        let mut in_scenario = false;

        for line in normalized.lines() {
            if let Some(title) = line.strip_prefix("### Requirement:") {
                blocks.push(RequirementBlock {
                    title: title.trim().to_string(),
                    body: String::new(),
                    scenarios: Vec::new(),
                });
                in_requirement = true;
                in_scenario = false;
                continue;
            }
            if let Some(title) = line.strip_prefix("#### Scenario:") {
                if !in_requirement {
                    return Err(OpenspecError::OrphanScenario {
                        spec: spec_path.to_string(),
                        title: title.trim().to_string(),
                    });
                }
                let block = blocks.last_mut().expect("in_requirement 保证有块");
                block.scenarios.push(ScenarioBlock {
                    title: title.trim().to_string(),
                    body: String::new(),
                });
                in_scenario = true;
                continue;
            }
            if let Some(block) = blocks.last_mut() {
                if in_scenario {
                    if let Some(scenario) = block.scenarios.last_mut() {
                        scenario.body.push_str(line);
                        scenario.body.push('\n');
                    }
                } else if in_requirement {
                    block.body.push_str(line);
                    block.body.push('\n');
                }
            }
        }

        if blocks.is_empty() {
            return Err(OpenspecError::NoRequirements(spec_path.to_string()));
        }

        let mut requirements: Vec<RequirementRef> = Vec::with_capacity(blocks.len());
        let mut scenarios: Vec<ScenarioRef> = Vec::new();
        for block in &blocks {
            let id = requirement_id(&block.title);
            if requirements.iter().any(|item| item.id == id) {
                return Err(OpenspecError::DuplicateRequirement {
                    spec: spec_path.to_string(),
                    title: block.title.clone(),
                });
            }
            // requirement 的正文摘要覆盖自身段落与全部 scenario，
            // 这样"只改一个 scenario"也会被判定为需求被改写。
            let mut digest_input = normalize_block(&block.body);
            for (ordinal, scenario) in block.scenarios.iter().enumerate() {
                let scenario_digest = sha256_hex(normalize_block(&scenario.body).as_bytes());
                scenarios.push(ScenarioRef {
                    spec_path: spec_path.to_string(),
                    requirement_id: id.clone(),
                    title: scenario.title.clone(),
                    ordinal,
                    text_digest: scenario_digest.clone(),
                });
                digest_input.push_str(&scenario.title);
                digest_input.push('\n');
                digest_input.push_str(&scenario_digest);
                digest_input.push('\n');
            }
            requirements.push(RequirementRef {
                spec_path: spec_path.to_string(),
                id,
                title: block.title.clone(),
                text_digest: sha256_hex(digest_input.as_bytes()),
            });
        }

        Ok(Self {
            revision: SpecRevision {
                spec_path: spec_path.to_string(),
                digest: spec_digest(text),
                requirement_count: requirements.len(),
                scenario_count: scenarios.len(),
                captured_at_ms,
            },
            requirements,
            scenarios,
        })
    }

    pub fn requirement(&self, id: &str) -> Option<&RequirementRef> {
        self.requirements.iter().find(|item| item.id == id)
    }

    pub fn scenarios_of(&self, requirement_id: &str) -> Vec<&ScenarioRef> {
        self.scenarios
            .iter()
            .filter(|item| item.requirement_id == requirement_id)
            .collect()
    }

    /// 引用是否仍然对应当前文件：需求标题还在、正文摘要未变。
    pub fn requirement_is_fresh(&self, id: &str, text: &str) -> bool {
        let Ok(current) = Self::parse(&self.revision.spec_path, text, 0) else {
            return false;
        };
        match (self.requirement(id), current.requirement(id)) {
            (Some(old), Some(new)) => old.text_digest == new.text_digest,
            _ => false,
        }
    }
}

/// 整份规格文本的规范化摘要（去行尾空白、压连续空行、统一换行）。
pub fn spec_digest(text: &str) -> String {
    sha256_hex(normalize_text(text).as_bytes())
}

fn normalize_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank_run = 0usize;
    for line in text.replace("\r\n", "\n").replace('\r', "\n").split('\n') {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            blank_run += 1;
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
            if blank_run > 0 {
                out.push('\n');
            }
        }
        blank_run = 0;
        out.push_str(trimmed);
    }
    out
}

fn normalize_block(text: &str) -> String {
    normalize_text(text).trim().to_string()
}

/// 标题 → 稳定键：去首尾空白、折叠内部空白、转小写；标点保留（中文标题里 `、` 有语义）。
fn requirement_id(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut last_space = false;
    for ch in title.chars() {
        if ch.is_whitespace() {
            if !last_space && !out.is_empty() {
                out.push(' ');
            }
            last_space = true;
        } else {
            last_space = false;
            for lower in ch.to_lowercase() {
                out.push(lower);
            }
        }
    }
    out.trim().to_string()
}

// ---------------------------------------------------------------------------
// Change 目录与工作包派生的输入
// ---------------------------------------------------------------------------

/// 一个 Change 对象：它属于哪个仓库、在哪个路径。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeRef {
    pub repository: String,
    pub path: String,
    pub title: String,
}

impl ChangeRef {
    /// 从目录名与 `proposal.md` 的一级标题得到 Change 身份。
    ///
    /// 不解析 proposal 全文：Change 的语义在 `specs/` 与 `tasks.md`，标题只用于展示。
    pub fn from_change_dir(repository: &str, dir: &Path) -> Result<Self, OpenspecError> {
        let name = dir
            .file_name()
            .map(|item| item.to_string_lossy().to_string())
            .ok_or_else(|| OpenspecError::IncompleteChange {
                path: dir.display().to_string(),
                missing: "目录名".to_string(),
            })?;
        let proposal = dir.join("proposal.md");
        let title = match fs::read_to_string(&proposal) {
            Ok(text) => text
                .lines()
                .find_map(|line| line.strip_prefix("# "))
                .map(|line| line.trim().to_string())
                .filter(|line| !line.is_empty())
                .unwrap_or_else(|| name.clone()),
            Err(_) => name.clone(),
        };
        Ok(Self {
            repository: repository.to_string(),
            path: dir.display().to_string(),
            title,
        })
    }
}

/// 一个 Change 的完整可派发输入：Change 身份 + 各规格快照 + 当前任务行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangeDocument {
    pub change: ChangeRef,
    pub spec_commit: String,
    pub specs: Vec<SpecSnapshot>,
    pub tasks: Vec<ParsedTask>,
    pub tasks_path: String,
}

impl ChangeDocument {
    /// 读取 `<dir>/specs/*/spec.md` 与 `<dir>/tasks.md`。
    ///
    /// `spec_commit` 由调用方给出（通常是 `git rev-parse HEAD`），本模块不调 git：
    /// 它只需要一个可追溯的标识，不需要判断工作区是否干净。
    pub fn load(
        repository: &str,
        dir: &Path,
        spec_commit: &str,
        captured_at_ms: i64,
    ) -> Result<Self, OpenspecError> {
        let change = ChangeRef::from_change_dir(repository, dir)?;
        let specs_dir = dir.join("specs");
        if !specs_dir.is_dir() {
            return Err(OpenspecError::IncompleteChange {
                path: dir.display().to_string(),
                missing: "specs/".to_string(),
            });
        }
        let mut spec_files: Vec<_> = fs::read_dir(&specs_dir)
            .map_err(|err| OpenspecError::Io(err.to_string()))?
            .filter_map(Result::ok)
            .map(|entry| entry.path().join("spec.md"))
            .filter(|path| path.is_file())
            .collect();
        spec_files.sort();
        let mut specs = Vec::with_capacity(spec_files.len());
        for path in &spec_files {
            let text =
                fs::read_to_string(path).map_err(|err| OpenspecError::Io(err.to_string()))?;
            let rel = path
                .strip_prefix(dir)
                .unwrap_or(path)
                .to_string_lossy()
                .to_string();
            specs.push(SpecSnapshot::parse(&rel, &text, captured_at_ms)?);
        }

        let tasks_path = dir.join("tasks.md");
        let tasks_text =
            fs::read_to_string(&tasks_path).map_err(|_| OpenspecError::IncompleteChange {
                path: dir.display().to_string(),
                missing: "tasks.md".to_string(),
            })?;

        Ok(Self {
            change,
            spec_commit: spec_commit.to_string(),
            specs,
            tasks: parse_tasks(&tasks_text),
            tasks_path: tasks_path.display().to_string(),
        })
    }

    /// 全部 requirement 的稳定键，用于覆盖检查。
    pub fn all_requirements(&self) -> Vec<(String, String)> {
        self.specs
            .iter()
            .flat_map(|spec| {
                spec.requirements
                    .iter()
                    .map(|req| (req.spec_path.clone(), req.id.clone()))
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// tasks.md 解析与稳定身份
// ---------------------------------------------------------------------------

/// tasks.md 里的一条任务。`display` 只用于展示，**不参与身份判定**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedTask {
    /// 最近的 `##` 标题，作为语义分组锚点（不是行号）。
    pub section: String,
    /// 显示编号（`1.2`），仅展示。
    pub display: Option<String>,
    /// 去掉勾选框与编号后的正文。
    pub text: String,
    pub checked: bool,
    /// 规范化正文的指纹；身份的唯一依据。
    pub fingerprint: String,
}

/// 解析 tasks.md：只认 `- [ ]` / `- [x]` 开头的行。
pub fn parse_tasks(text: &str) -> Vec<ParsedTask> {
    let mut section = String::new();
    let mut out = Vec::new();
    for line in text.replace("\r\n", "\n").split('\n') {
        let trimmed = line.trim();
        if let Some(heading) = trimmed.strip_prefix("## ") {
            section = heading.trim().to_string();
            continue;
        }
        if let Some(heading) = trimmed.strip_prefix("### ") {
            // 三级标题不重置分组，但记进锚点，避免"同一节下两个子节"被当成同一组
            section = format!("{section} / {}", heading.trim());
            continue;
        }
        let Some(rest) = trimmed.strip_prefix("- [") else {
            continue;
        };
        let mut chars = rest.chars();
        let checked = match chars.next() {
            Some('x') | Some('X') => true,
            Some(' ') => false,
            _ => continue,
        };
        if chars.next() != Some(']') {
            continue;
        }
        let body = chars.as_str().trim();
        let (display, text) = split_display_number(body);
        let fingerprint = sha256_hex(normalize_task_text(&text).as_bytes());
        out.push(ParsedTask {
            section: section.clone(),
            display,
            text,
            checked,
            fingerprint,
        });
    }
    out
}

/// 拆出显示编号：`1.2 实现…` → (`Some("1.2")`, `实现…`)。没有编号时原样返回。
fn split_display_number(body: &str) -> (Option<String>, String) {
    let mut digits = String::new();
    let mut chars = body.char_indices().peekable();
    while let Some((_, ch)) = chars.peek().copied() {
        if ch.is_ascii_digit() || ch == '.' {
            digits.push(ch);
            chars.next();
        } else {
            break;
        }
    }
    // 编号必须至少有数字，且后面隔一个空格才是正文；否则整行都是正文。
    let has_digit = digits.chars().any(|ch| ch.is_ascii_digit());
    let rest_index = digits.len();
    let rest = &body[rest_index..];
    if has_digit && rest.starts_with(' ') {
        (Some(digits), rest.trim().to_string())
    } else {
        (None, body.to_string())
    }
}

/// 任务正文的规范化：折叠空白、去尾部标点差异之外的东西一律不动。
/// 只做"排版不该影响身份"的那部分归一化，措辞变化必须留下痕迹。
fn normalize_task_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last_space = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !last_space {
                out.push(' ');
            }
            last_space = true;
        } else {
            last_space = false;
            out.push(ch);
        }
    }
    out.trim().to_string()
}

/// 任务映射的匹配状态。`Unresolved` 是**要求人工确认**的状态，不是错误，也不是新任务。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MatchState {
    /// 正文与分组都没变。
    Matched,
    /// 正文相同但换了分组（重排/移动）。
    Moved,
    /// 正文变了且无法唯一匹配：必须人工确认，不能自动继承旧 ID。
    Unresolved {
        candidate: Option<String>,
        reason: String,
    },
    /// 上一版没有的任务。
    Added,
    /// 本版不存在的任务（保留在结果里用于报告，不参与派发）。
    Removed,
}

impl MatchState {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Matched => "matched",
            Self::Moved => "moved",
            Self::Unresolved { .. } => "unresolved",
            Self::Added => "added",
            Self::Removed => "removed",
        }
    }

    pub fn needs_human(&self) -> bool {
        matches!(self, Self::Unresolved { .. })
    }
}

/// 一条任务映射记录。字段与规格「映射记录来源路径、锚点、指纹与匹配状态」一一对应。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskMapping {
    pub task_id: String,
    pub source_path: String,
    pub section: String,
    pub display: Option<String>,
    pub fingerprint: String,
    pub state: MatchState,
}

/// 重映射结果。`retired` 是被判定为消失的旧映射，报告里要点名，不能悄悄丢掉。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemapOutcome {
    pub mappings: Vec<TaskMapping>,
    pub retired: Vec<TaskMapping>,
    pub unresolved: Vec<TaskMapping>,
}

/// 稳定 ID：由指纹派生，因此与顺序、行号、显示编号都无关。
pub fn task_id_for(fingerprint: &str) -> String {
    format!("task-{}", &fingerprint[..24])
}

/// 把当前 tasks.md 映射到上一版映射上。
///
/// 规则（对应规格「稳定任务身份」）：
/// 1. 指纹唯一命中 → 复用旧 ID；分组变了标 `Moved`，没变标 `Matched`。
/// 2. 未命中且旧版恰好剩一条、当前恰好剩一条、且两者同属一个分组 → `Unresolved`，
///    附候选旧 ID，**不自动绑定**（语义变了要人确认是不是同一条）。
/// 3. 其余未命中 → `Added`（新 ID）。
/// 4. 旧版里没被命中的 → `Removed`。
///
/// 任何一步都不读行号或显示编号；测试 `never_rebinds_by_display_number` 覆盖这条。
pub fn remap_tasks(
    previous: &[TaskMapping],
    source_path: &str,
    current: &[ParsedTask],
) -> RemapOutcome {
    let mut by_fingerprint: HashMap<&str, Vec<usize>> = HashMap::new();
    for (index, item) in previous.iter().enumerate() {
        by_fingerprint
            .entry(item.fingerprint.as_str())
            .or_default()
            .push(index);
    }

    let mut used = vec![false; previous.len()];
    let mut slots: Vec<Option<TaskMapping>> = vec![None; current.len()];

    // 第一轮：指纹唯一命中。重复指纹（同一正文出现两次）不参与本轮，留给第二轮判歧义。
    for (index, task) in current.iter().enumerate() {
        let Some(candidates) = by_fingerprint.get(task.fingerprint.as_str()) else {
            continue;
        };
        if candidates.len() != 1 {
            continue;
        }
        let prev_index = candidates[0];
        if used[prev_index] {
            continue;
        }
        used[prev_index] = true;
        let old = &previous[prev_index];
        let state = if old.section == task.section {
            MatchState::Matched
        } else {
            MatchState::Moved
        };
        slots[index] = Some(TaskMapping {
            task_id: old.task_id.clone(),
            source_path: source_path.to_string(),
            section: task.section.clone(),
            display: task.display.clone(),
            fingerprint: task.fingerprint.clone(),
            state,
        });
    }

    // 第二轮：剩余项。
    //
    // 判断只在**同一分组内**做，且只做"能不能唯一配对"，绝不用位置或显示编号补位：
    // - 同组恰好一对一 → Unresolved（带候选旧 ID）：像改写，但要人确认；
    // - 同组多对多 → Unresolved（无候选）：知道是这一组变了，却指不出谁对应谁；
    // - 同组没有剩余旧条目 → Added：这一组确实多了新任务。
    let leftover_previous: Vec<usize> = (0..previous.len()).filter(|i| !used[*i]).collect();
    let leftover_current: Vec<usize> = (0..current.len()).filter(|i| slots[*i].is_none()).collect();

    for index in &leftover_current {
        let task = &current[*index];
        let same_section_previous: Vec<usize> = leftover_previous
            .iter()
            .copied()
            .filter(|i| !used[*i] && previous[*i].section == task.section)
            .collect();
        let same_section_current = leftover_current
            .iter()
            .filter(|i| slots[**i].is_none() && current[**i].section == task.section)
            .count();

        let state = if same_section_previous.is_empty() {
            MatchState::Added
        } else if same_section_previous.len() == 1 && same_section_current == 1 {
            let old = &previous[same_section_previous[0]];
            used[same_section_previous[0]] = true;
            MatchState::Unresolved {
                candidate: Some(old.task_id.clone()),
                reason: "正文已变化且同组只剩这一条：需确认是改写还是新任务".to_string(),
            }
        } else {
            MatchState::Unresolved {
                candidate: None,
                reason: format!(
                    "同组内 {} 条旧任务与 {} 条新正文同时变化，无法唯一匹配：需人工确认",
                    same_section_previous.len(),
                    same_section_current
                ),
            }
        };
        slots[*index] = Some(TaskMapping {
            task_id: task_id_for(&task.fingerprint),
            source_path: source_path.to_string(),
            section: task.section.clone(),
            display: task.display.clone(),
            fingerprint: task.fingerprint.clone(),
            state,
        });
    }

    let mappings: Vec<TaskMapping> = slots.into_iter().flatten().collect();
    let retired: Vec<TaskMapping> = previous
        .iter()
        .enumerate()
        .filter(|(index, _)| !used[*index])
        .map(|(_, item)| TaskMapping {
            state: MatchState::Removed,
            ..item.clone()
        })
        .collect();
    let unresolved = mappings
        .iter()
        .filter(|item| item.state.needs_human())
        .cloned()
        .collect();

    RemapOutcome {
        mappings,
        retired,
        unresolved,
    }
}

// ---------------------------------------------------------------------------
// 工作包拆分与覆盖检查
// ---------------------------------------------------------------------------

/// 拆分依据。机械均分（按 Agent 数量、按行范围）一律拒绝——它不保证可独立验收。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplitRationale {
    /// 按业务行为、可独立验收与交付边界。
    Behaviour,
    /// 按 Agent 数量平均：拒绝。
    AgentCount,
    /// 按行范围切：拒绝。
    LineRange,
    /// 调用方自述为人工划分；仍需通过覆盖与验收检查。
    Manual,
}

/// 一份工作包草案。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkPackageDraft {
    pub id: String,
    /// 这条工作包交付的一个**可观察行为**。
    pub behaviour: String,
    /// 独立验收条件；空表示"拆得不对"，不是"以后补"。
    pub acceptance: String,
    pub task_ids: Vec<String>,
    pub requirements: Vec<String>,
    pub depends_on: Vec<String>,
    pub shared_resources: Vec<String>,
    pub estimated_files: u32,
}

/// 拆分检查发现的问题。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SplitProblem {
    /// 仅按 Agent 数量或行范围均分。
    MechanicalSplit(&'static str),
    /// 缺少可独立验收条件。
    MissingAcceptance(String),
    /// 缺少可观察行为描述。
    MissingBehaviour(String),
    /// 没有关联任何任务。
    EmptyPackage(String),
    /// 超过单包文件数上限。
    TooLarge { package: String, files: u32 },
    /// 依赖成环。
    Cycle(Vec<String>),
    /// 多个包声明同一共享资源。
    SharedResourceConflict {
        resource: String,
        packages: Vec<String>,
    },
    /// 需求没被任何工作包覆盖。
    MissingCoverage(Vec<String>),
    /// 引用了不存在的需求。
    UnknownRequirement {
        package: String,
        requirement: String,
    },
    /// 引用了不存在的任务 ID。
    UnknownTask { package: String, task_id: String },
    /// 还有未确认的任务映射，不能据此拆包。
    UnresolvedTasks(Vec<String>),
}

impl fmt::Display for SplitProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MechanicalSplit(kind) => write!(f, "机械均分（{kind}）：须按可验证输出重新划分"),
            Self::MissingAcceptance(pkg) => write!(f, "工作包 {pkg} 没有可独立验收条件"),
            Self::MissingBehaviour(pkg) => write!(f, "工作包 {pkg} 没有可观察行为描述"),
            Self::EmptyPackage(pkg) => write!(f, "工作包 {pkg} 没有关联任务"),
            Self::TooLarge { package, files } => {
                write!(f, "工作包 {package} 声明 {files} 个文件，超过单包上限")
            }
            Self::Cycle(nodes) => write!(f, "依赖成环：{}", nodes.join(" → ")),
            Self::SharedResourceConflict { resource, packages } => write!(
                f,
                "共享资源 `{resource}` 被 {} 同时修改，需责任者协调",
                packages.join(", ")
            ),
            Self::MissingCoverage(items) => {
                write!(f, "以下需求未被任何工作包覆盖：{}", items.join(", "))
            }
            Self::UnknownRequirement {
                package,
                requirement,
            } => write!(f, "工作包 {package} 引用了不存在的需求 `{requirement}`"),
            Self::UnknownTask { package, task_id } => {
                write!(f, "工作包 {package} 引用了不存在的任务 `{task_id}`")
            }
            Self::UnresolvedTasks(items) => write!(
                f,
                "还有 {} 条任务映射未确认（{}），不能据此拆包",
                items.len(),
                items.join(", ")
            ),
        }
    }
}

/// 单包文件数上限。超过说明"这是一个切片而不是一个工作包"。
pub const MAX_FILES_PER_PACKAGE: u32 = 12;

/// 拆分计划。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SplitPlan {
    pub rationale: SplitRationale,
    pub packages: Vec<WorkPackageDraft>,
}

/// 检查一份拆分计划。返回空表示可派发。
pub fn check_split(
    plan: &SplitPlan,
    document_specs: &[SpecSnapshot],
    known_task_ids: &BTreeSet<String>,
    unresolved: &[TaskMapping],
) -> Vec<SplitProblem> {
    let mut problems = Vec::new();

    if let SplitRationale::AgentCount = plan.rationale {
        problems.push(SplitProblem::MechanicalSplit("按 Agent 数量"));
    }
    if let SplitRationale::LineRange = plan.rationale {
        problems.push(SplitProblem::MechanicalSplit("按行范围"));
    }
    if !unresolved.is_empty() {
        problems.push(SplitProblem::UnresolvedTasks(
            unresolved.iter().map(|item| item.task_id.clone()).collect(),
        ));
    }

    // 所有已知需求键（spec/requirement 两级都算，允许只写 requirement 名）
    let mut known_requirements: HashSet<String> = HashSet::new();
    for spec in document_specs {
        for req in &spec.requirements {
            known_requirements.insert(req.id.clone());
            known_requirements.insert(format!("{}/{}", spec.revision.spec_path, req.id));
        }
    }

    let mut covered: BTreeSet<String> = BTreeSet::new();
    for package in &plan.packages {
        if package.behaviour.trim().is_empty() {
            problems.push(SplitProblem::MissingBehaviour(package.id.clone()));
        }
        if package.acceptance.trim().is_empty() {
            problems.push(SplitProblem::MissingAcceptance(package.id.clone()));
        }
        if package.task_ids.is_empty() {
            problems.push(SplitProblem::EmptyPackage(package.id.clone()));
        }
        if package.estimated_files > MAX_FILES_PER_PACKAGE {
            problems.push(SplitProblem::TooLarge {
                package: package.id.clone(),
                files: package.estimated_files,
            });
        }
        for task_id in &package.task_ids {
            if !known_task_ids.contains(task_id) {
                problems.push(SplitProblem::UnknownTask {
                    package: package.id.clone(),
                    task_id: task_id.clone(),
                });
            }
        }
        for requirement in &package.requirements {
            if !known_requirements.contains(requirement) {
                problems.push(SplitProblem::UnknownRequirement {
                    package: package.id.clone(),
                    requirement: requirement.clone(),
                });
                continue;
            }
            // 统一记成 requirement 名，便于覆盖比对
            let bare = requirement.rsplit('/').next().unwrap_or(requirement);
            covered.insert(bare.to_string());
        }
    }

    // 覆盖：所有 requirement 至少被一个包引用。
    let missing: Vec<String> = document_specs
        .iter()
        .flat_map(|spec| spec.requirements.iter())
        .filter(|req| !covered.contains(&req.id))
        .map(|req| req.id.clone())
        .collect();
    if !missing.is_empty() {
        problems.push(SplitProblem::MissingCoverage(missing));
    }

    // 共享资源冲突：同一资源被两个包声明即需协调（哪怕改的是不同行）。
    let mut by_resource: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for package in &plan.packages {
        for resource in &package.shared_resources {
            by_resource
                .entry(resource.as_str())
                .or_default()
                .push(package.id.as_str());
        }
    }
    for (resource, mut packages) in by_resource {
        if packages.len() > 1 {
            packages.sort_unstable();
            problems.push(SplitProblem::SharedResourceConflict {
                resource: resource.to_string(),
                packages: packages.into_iter().map(str::to_string).collect(),
            });
        }
    }

    if let Some(cycle) = find_cycle(&plan.packages) {
        problems.push(SplitProblem::Cycle(cycle));
    }

    problems
}

/// 依赖图找环（DFS 三色）。返回环上的节点序列，便于直接展示。
fn find_cycle(packages: &[WorkPackageDraft]) -> Option<Vec<String>> {
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        White,
        Grey,
        Black,
    }
    let index: HashMap<&str, usize> = packages
        .iter()
        .enumerate()
        .map(|(i, item)| (item.id.as_str(), i))
        .collect();
    let mut marks = vec![Mark::White; packages.len()];
    let mut stack: Vec<usize> = Vec::new();

    fn visit(
        node: usize,
        packages: &[WorkPackageDraft],
        index: &HashMap<&str, usize>,
        marks: &mut Vec<Mark>,
        stack: &mut Vec<usize>,
    ) -> Option<Vec<String>> {
        marks[node] = Mark::Grey;
        stack.push(node);
        for dep in &packages[node].depends_on {
            let Some(&next) = index.get(dep.as_str()) else {
                continue;
            };
            match marks[next] {
                Mark::Grey => {
                    let start = stack.iter().position(|item| *item == next).unwrap_or(0);
                    let mut cycle: Vec<String> = stack[start..]
                        .iter()
                        .map(|item| packages[*item].id.clone())
                        .collect();
                    cycle.push(packages[next].id.clone());
                    return Some(cycle);
                }
                Mark::White => {
                    if let Some(cycle) = visit(next, packages, index, marks, stack) {
                        return Some(cycle);
                    }
                }
                Mark::Black => {}
            }
        }
        stack.pop();
        marks[node] = Mark::Black;
        None
    }

    for node in 0..packages.len() {
        if marks[node] == Mark::White
            && let Some(cycle) = visit(node, packages, &index, &mut marks, &mut stack)
        {
            return Some(cycle);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// 进度投影（勾选 ≠ 验收）
// ---------------------------------------------------------------------------

/// 一次勾选观察。它是**观察**，不是证据。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgressObservation {
    pub task_id: String,
    pub checked: bool,
    pub source_path: String,
    pub observed_at_ms: i64,
}

impl ProgressObservation {
    /// 恒为 false。写成方法而不是注释，是为了让"想拿它当证据"的调用点在编译期就要显式绕过。
    pub fn is_acceptance_evidence(&self) -> bool {
        false
    }
}

/// 工作包状态：只有宿主接受决定能把它变成 `accepted`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkPackageStatus {
    pub package_id: String,
    pub accepted: bool,
    pub accepted_by: Option<String>,
}

/// 把勾选观察投影到工作包上。
///
/// 投影只影响"进度百分比"这类展示字段，**不改变** `accepted`。函数签名里接收而不再返回
/// `accepted`，是为了让这件事在类型层面就成立，而不是靠调用方自觉。
pub fn project_progress(
    statuses: &[WorkPackageStatus],
    observations: &[ProgressObservation],
) -> Vec<(String, usize)> {
    // 观察里的 task 与包的对应关系由调用方持有；这里只统计"被观察到的勾选数"，
    // 并显式忽略任何试图据此改 `accepted` 的推断。
    let _ = statuses;
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for observation in observations {
        if observation.checked && !observation.is_acceptance_evidence() {
            *counts.entry(observation.source_path.clone()).or_default() += 1;
        }
    }
    counts.into_iter().collect()
}

/// 导入外部手工勾选。返回观察，不返回状态变更。
pub fn import_checkbox_observations(
    mappings: &[TaskMapping],
    tasks: &[ParsedTask],
    tasks_path: &str,
    observed_at_ms: i64,
) -> Vec<ProgressObservation> {
    let mut by_fingerprint: HashMap<&str, &TaskMapping> = HashMap::new();
    for mapping in mappings {
        by_fingerprint.insert(mapping.fingerprint.as_str(), mapping);
    }
    tasks
        .iter()
        .filter_map(|task| {
            let mapping = by_fingerprint.get(task.fingerprint.as_str())?;
            Some(ProgressObservation {
                task_id: mapping.task_id.clone(),
                checked: task.checked,
                source_path: tasks_path.to_string(),
                observed_at_ms,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 单一归档入口
// ---------------------------------------------------------------------------

/// 规格/实现一致性检查结果。未跑与失败都不允许归档。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConsistencyCheck {
    Passed { note: String },
    NotRun,
    Failed(String),
}

/// 一个 Change 的归档申请。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveRequest {
    pub change_id: String,
    /// 该 Change 修改的 requirement 稳定键（跨 spec 用 `path/req` 形式）。
    pub requirements: Vec<String>,
    /// 集成验收是否通过（合并后的组合，不是单分支绿灯）。
    pub integration_accepted: bool,
    pub consistency: ConsistencyCheck,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveStep {
    pub change_id: String,
    pub position: usize,
    /// 这一行为什么要等前面：共享了哪些 requirement。
    pub waits_for: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchivePlan {
    pub steps: Vec<ArchiveStep>,
    /// 有共享 requirement 时为 false：不允许并行写入现行规格。
    pub parallel_safe: bool,
    pub shared_requirements: Vec<String>,
}

/// 规划归档顺序。
///
/// 规则：两个 Change 改同一 requirement，即便 Git 文本无冲突，也**必须串行**——
/// 因为规格合并的冲突不在文本层，而在"同一句需求被两处改写"的语义层。
pub fn plan_archive(requests: &[ArchiveRequest]) -> Result<ArchivePlan, OpenspecError> {
    for request in requests {
        if !request.integration_accepted {
            return Err(OpenspecError::NotReadyToArchive {
                change_id: request.change_id.clone(),
                why: "集成验收未通过；单分支绿灯不算".to_string(),
            });
        }
        match &request.consistency {
            ConsistencyCheck::Passed { .. } => {}
            ConsistencyCheck::NotRun => {
                return Err(OpenspecError::NotReadyToArchive {
                    change_id: request.change_id.clone(),
                    why: "规格/实现一致性检查未运行".to_string(),
                });
            }
            ConsistencyCheck::Failed(reason) => {
                return Err(OpenspecError::NotReadyToArchive {
                    change_id: request.change_id.clone(),
                    why: format!("一致性检查失败：{reason}"),
                });
            }
        }
    }

    // requirement → 修改它的 Change
    let mut owners: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for request in requests {
        for requirement in &request.requirements {
            owners
                .entry(requirement.as_str())
                .or_default()
                .push(request.change_id.as_str());
        }
    }
    let shared: Vec<String> = owners
        .iter()
        .filter(|(_, changes)| changes.len() > 1)
        .map(|(requirement, _)| (*requirement).to_string())
        .collect();

    let mut steps = Vec::with_capacity(requests.len());
    let mut seen: Vec<&str> = Vec::new();
    for (position, request) in requests.iter().enumerate() {
        // 只等"共享了 requirement 且排在自己前面"的 Change；不共享的可以并行，但本实现
        // 输出的是串行序列，`parallel_safe` 说明调用方能否安全地并行执行互不相干的部分。
        let waits_for: Vec<String> = requests[..position]
            .iter()
            .filter(|earlier| {
                earlier
                    .requirements
                    .iter()
                    .any(|req| request.requirements.contains(req))
            })
            .map(|earlier| earlier.change_id.clone())
            .collect();
        seen.push(request.change_id.as_str());
        steps.push(ArchiveStep {
            change_id: request.change_id.clone(),
            position,
            waits_for,
        });
    }

    Ok(ArchivePlan {
        steps,
        parallel_safe: shared.is_empty(),
        shared_requirements: shared,
    })
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC: &str = r#"# Spec Delta: openspec-integration

## Purpose

示例用途。

## ADDED Requirements

### Requirement: 变更与规格修订映射

系统 SHALL 把 Change 目录映射到 Change 对象。

#### Scenario: 工作包派生
- **WHEN** 一个已批准 Change 产生工作包
- **THEN** 工作包记录 spec_commit

### Requirement: 稳定任务身份

tasks.md 条目 SHALL 映射到稳定 TaskDefinitionRef。

#### Scenario: 任务重排
- **WHEN** tasks.md 条目顺序变化
- **THEN** 稳定 ID 保持不变
"#;

    const TASKS: &str = r#"# Tasks: 示例

## 1. 第一组

- [ ] 1.1 实现规格映射
- [ ] 1.2 实现任务身份

## 2. 第二组

- [ ] 2.1 实现工作包拆分
- [x] 2.2 编写说明
"#;

    fn snapshot() -> SpecSnapshot {
        SpecSnapshot::parse("specs/openspec-integration/spec.md", SPEC, 1_000).unwrap()
    }

    #[test]
    fn requirements_and_scenarios_are_keyed_by_title_not_line() {
        let snap = snapshot();
        assert_eq!(snap.requirements.len(), 2);
        assert_eq!(snap.scenarios.len(), 2);
        assert_eq!(snap.requirements[0].id, "变更与规格修订映射");
        assert_eq!(snap.requirements[1].id, "稳定任务身份");
        // 引用带上 spec 路径，跨文件同名标题不会混。
        assert_eq!(
            snap.requirements[0].spec_path,
            "specs/openspec-integration/spec.md"
        );
        let scenarios = snap.scenarios_of("稳定任务身份");
        assert_eq!(scenarios.len(), 1);
        assert_eq!(scenarios[0].title, "任务重排");
        assert_eq!(snap.revision.requirement_count, 2);
        assert_eq!(snap.revision.scenario_count, 2);
    }

    #[test]
    fn work_package_keeps_its_revision_when_the_file_changes() {
        let snap = snapshot();
        let digest_before = snap.revision.digest.clone();
        assert!(snap.revision.matches_file(SPEC));

        // 在文件前面插一段：行号全变，但修订摘要不因此"自动跟随"。
        let edited = format!("<!-- 插一行 -->\n{SPEC}");
        assert!(!snap.revision.matches_file(&edited));
        assert_eq!(
            snap.revision.digest, digest_before,
            "已记录的修订不能被磁盘内容改写"
        );
        assert_eq!(
            snap.revision.digest_ref(),
            format!("sha256:{digest_before}")
        );

        // 纯排版变化（行尾空白）不算漂移。
        let reformatted = SPEC.replace("\n", "  \n");
        assert!(snap.revision.matches_file(&reformatted));
    }

    #[test]
    fn requirement_is_fresh_only_while_its_text_is_unchanged() {
        let snap = snapshot();
        assert!(snap.requirement_is_fresh("稳定任务身份", SPEC));
        let rewritten = SPEC.replace("SHALL 映射到稳定 TaskDefinitionRef", "SHALL 按行号映射");
        assert!(
            !snap.requirement_is_fresh("稳定任务身份", &rewritten),
            "需求正文被改写后，旧引用必须报不新鲜"
        );
        assert!(
            snap.requirement_is_fresh("变更与规格修订映射", &rewritten),
            "没被改的另一条需求仍应新鲜"
        );
    }

    #[test]
    fn duplicate_requirement_titles_are_rejected() {
        let duplicated = format!("{SPEC}\n### Requirement: 稳定任务身份\n再次出现\n");
        let err = SpecSnapshot::parse("spec.md", &duplicated, 0).unwrap_err();
        assert!(
            matches!(err, OpenspecError::DuplicateRequirement { .. }),
            "实际 {err:?}"
        );
    }

    #[test]
    fn scenarios_without_requirement_are_rejected() {
        let orphan = "#### Scenario: 孤零零\n- **WHEN** x\n";
        let err = SpecSnapshot::parse("spec.md", orphan, 0).unwrap_err();
        assert!(
            matches!(err, OpenspecError::OrphanScenario { .. }),
            "实际 {err:?}"
        );
    }

    #[test]
    fn reordering_tasks_keeps_stable_ids() {
        let tasks = parse_tasks(TASKS);
        assert_eq!(tasks.len(), 4);
        assert_eq!(tasks[0].display.as_deref(), Some("1.1"));
        assert_eq!(tasks[0].text, "实现规格映射");

        let first = remap_tasks(&[], "tasks.md", &tasks);
        assert_eq!(first.mappings.len(), 4);
        // 首次映射没有"上一版"，所以每条都记为 Added：这是首次进入映射表，不是异常状态。
        assert!(
            first
                .mappings
                .iter()
                .all(|item| item.state == MatchState::Added)
        );
        let ids: Vec<String> = first
            .mappings
            .iter()
            .map(|item| item.task_id.clone())
            .collect();

        // 交换两组顺序（正文不变）
        let reordered = TASKS.replace(
            "## 1. 第一组\n\n- [ ] 1.1 实现规格映射\n- [ ] 1.2 实现任务身份\n\n## 2. 第二组\n\n- [ ] 2.1 实现工作包拆分\n- [x] 2.2 编写说明\n",
            "## 2. 第二组\n\n- [ ] 2.1 实现工作包拆分\n- [x] 2.2 编写说明\n\n## 1. 第一组\n\n- [ ] 1.1 实现规格映射\n- [ ] 1.2 实现任务身份\n",
        );
        let tasks2 = parse_tasks(&reordered);
        let second = remap_tasks(&first.mappings, "tasks.md", &tasks2);
        assert!(second.unresolved.is_empty(), "重排不应产生未确认项");
        assert!(second.retired.is_empty());
        let ids2: Vec<String> = second
            .mappings
            .iter()
            .map(|item| item.task_id.clone())
            .collect();
        // 顺序变了，所以比"集合"而不是"序列"：身份与位置无关，这正是要证明的。
        let mut sorted_ids = ids.clone();
        sorted_ids.sort();
        let mut sorted_ids2 = ids2.clone();
        sorted_ids2.sort();
        assert_eq!(sorted_ids, sorted_ids2, "ID 必须随正文走，不随位置走");
        // 整组搬家时分组标签跟着走，所以状态仍是 Matched；ID 才是重排不变的那部分。
        assert!(
            second
                .mappings
                .iter()
                .all(|item| item.state == MatchState::Matched)
        );

        // 只把一条挪到另一个分组：这条应标 Moved，其余仍 Matched。
        let moved = TASKS.replace("- [ ] 1.2 实现任务身份\n", "")
            + "\n## 3. 第三组\n\n- [ ] 1.2 实现任务身份\n";
        let third = remap_tasks(&first.mappings, "tasks.md", &parse_tasks(&moved));
        let moved_item = third
            .mappings
            .iter()
            .find(|item| item.display.as_deref() == Some("1.2"))
            .expect("应能找到被移动的任务");
        assert_eq!(moved_item.state, MatchState::Moved, "跨分组移动应标 Moved");
        assert!(third.unresolved.is_empty());
    }

    #[test]
    fn changed_task_text_becomes_unresolved_instead_of_rebinding() {
        let tasks = parse_tasks(TASKS);
        let first = remap_tasks(&[], "tasks.md", &tasks);

        // 只改一条的正文
        let edited = TASKS.replace("2.1 实现工作包拆分", "2.1 实现工作包拆分并加覆盖检查");
        let tasks2 = parse_tasks(&edited);
        let second = remap_tasks(&first.mappings, "tasks.md", &tasks2);

        assert_eq!(second.unresolved.len(), 1, "改语义应进未确认，不能悄悄绑定");
        let item = &second.unresolved[0];
        match &item.state {
            MatchState::Unresolved { candidate, reason } => {
                assert!(candidate.is_some(), "未确认项要带候选旧 ID 供人判断");
                assert!(reason.contains("确认"), "理由要说清需要人确认什么");
            }
            other => panic!("应为 Unresolved，实际 {other:?}"),
        }
        // 新 ID 与旧 ID 不同：绝不自动继承
        assert!(
            second
                .mappings
                .iter()
                .all(|m| m.task_id != first.mappings[2].task_id)
        );
        // 其它三条仍然是同一个 ID
        assert_eq!(second.retired.len(), 0);
    }

    #[test]
    fn never_rebinds_by_display_number() {
        // 把两条任务的编号互换、正文都改掉：若按编号绑定，就会张冠李戴。
        let tasks = parse_tasks("- [ ] 1.1 甲任务\n- [ ] 1.2 乙任务\n");
        let first = remap_tasks(&[], "tasks.md", &tasks);
        let edited = parse_tasks("- [ ] 1.1 乙任务（改）\n- [ ] 1.2 甲任务（改）\n");
        let second = remap_tasks(&first.mappings, "tasks.md", &edited);
        // 同组一对一，两条都会进 Unresolved —— 关键是没有任何一条继承了旧 ID。
        assert_eq!(second.unresolved.len(), 2);
        let old_ids: BTreeSet<&str> = first.mappings.iter().map(|m| m.task_id.as_str()).collect();
        assert!(
            second
                .mappings
                .iter()
                .all(|m| !old_ids.contains(m.task_id.as_str())),
            "编号相同也不能据此继承身份"
        );
    }

    #[test]
    fn removed_task_is_reported_not_silently_dropped() {
        let tasks = parse_tasks(TASKS);
        let first = remap_tasks(&[], "tasks.md", &tasks);
        let fewer = TASKS.replace("- [x] 2.2 编写说明\n", "");
        let second = remap_tasks(&first.mappings, "tasks.md", &parse_tasks(&fewer));
        assert_eq!(second.retired.len(), 1);
        assert_eq!(second.retired[0].state, MatchState::Removed);
    }

    #[test]
    fn checkbox_import_is_observation_and_never_accepts() {
        let tasks = parse_tasks(TASKS);
        let mappings = remap_tasks(&[], "tasks.md", &tasks).mappings;
        let observations = import_checkbox_observations(&mappings, &tasks, "tasks.md", 5_000);
        assert_eq!(observations.len(), 4);
        assert!(
            observations
                .iter()
                .all(|item| !item.is_acceptance_evidence())
        );

        let statuses = vec![WorkPackageStatus {
            package_id: "pkg-1".to_string(),
            accepted: false,
            accepted_by: None,
        }];
        let projection = project_progress(&statuses, &observations);
        // 投影产出的是"勾了几条"，不产出 accepted。
        assert_eq!(projection.len(), 1);
        assert_eq!(projection[0].1, 1, "只有 2.2 是勾上的");
        assert!(!statuses[0].accepted, "勾选不得把工作包变成已接受");
    }

    fn base_plan() -> (SplitPlan, Vec<SpecSnapshot>, BTreeSet<String>) {
        let snap = snapshot();
        let tasks = parse_tasks(TASKS);
        let mappings = remap_tasks(&[], "tasks.md", &tasks).mappings;
        let ids: BTreeSet<String> = mappings.iter().map(|m| m.task_id.clone()).collect();
        let mut sorted: Vec<String> = ids.iter().cloned().collect();
        sorted.sort();
        let plan = SplitPlan {
            rationale: SplitRationale::Behaviour,
            packages: vec![
                WorkPackageDraft {
                    id: "pkg-spec".to_string(),
                    behaviour: "规格与任务映射可用".to_string(),
                    acceptance: "两条命名用例通过".to_string(),
                    task_ids: vec![sorted[0].clone(), sorted[1].clone()],
                    requirements: vec!["变更与规格修订映射".to_string()],
                    depends_on: vec![],
                    shared_resources: vec![],
                    estimated_files: 3,
                },
                WorkPackageDraft {
                    id: "pkg-split".to_string(),
                    behaviour: "拆分检查可拒绝机械均分".to_string(),
                    acceptance: "覆盖/循环/冲突各有用例".to_string(),
                    task_ids: vec![sorted[2].clone(), sorted[3].clone()],
                    requirements: vec!["稳定任务身份".to_string()],
                    depends_on: vec!["pkg-spec".to_string()],
                    shared_resources: vec![],
                    estimated_files: 2,
                },
            ],
        };
        (plan, vec![snap], ids)
    }

    #[test]
    fn a_behaviour_based_plan_passes_every_check() {
        let (plan, specs, ids) = base_plan();
        let problems = check_split(&plan, &specs, &ids, &[]);
        assert!(problems.is_empty(), "预期无问题，实际 {problems:?}");
    }

    #[test]
    fn mechanical_split_by_agent_count_is_rejected() {
        let (mut plan, specs, ids) = base_plan();
        plan.rationale = SplitRationale::AgentCount;
        let problems = check_split(&plan, &specs, &ids, &[]);
        assert!(
            problems
                .iter()
                .any(|p| matches!(p, SplitProblem::MechanicalSplit(_))),
            "按 Agent 数量均分必须被拒，实际 {problems:?}"
        );

        plan.rationale = SplitRationale::LineRange;
        let problems = check_split(&plan, &specs, &ids, &[]);
        assert!(
            problems
                .iter()
                .any(|p| matches!(p, SplitProblem::MechanicalSplit(_)))
        );
    }

    #[test]
    fn missing_coverage_cycle_and_shared_conflict_are_all_reported() {
        let (mut plan, specs, ids) = base_plan();
        // 去掉第二条包的需求覆盖
        plan.packages[1].requirements.clear();
        // 互相依赖成环
        plan.packages[0].depends_on = vec!["pkg-split".to_string()];
        plan.packages[1].depends_on = vec!["pkg-spec".to_string()];
        // 同一共享资源
        plan.packages[0].shared_resources = vec!["crates/bod-app/migrations/".to_string()];
        plan.packages[1].shared_resources = vec!["crates/bod-app/migrations/".to_string()];

        let problems = check_split(&plan, &specs, &ids, &[]);
        assert!(
            problems
                .iter()
                .any(|p| matches!(p, SplitProblem::MissingCoverage(_))),
            "缺覆盖要报"
        );
        assert!(
            problems.iter().any(|p| matches!(p, SplitProblem::Cycle(_))),
            "成环要报，实际 {problems:?}"
        );
        assert!(
            problems
                .iter()
                .any(|p| matches!(p, SplitProblem::SharedResourceConflict { .. })),
            "共享冲突要报"
        );
    }

    #[test]
    fn package_without_independent_acceptance_is_rejected() {
        let (mut plan, specs, ids) = base_plan();
        plan.packages[0].acceptance = "  ".to_string();
        plan.packages[1].behaviour = String::new();
        let problems = check_split(&plan, &specs, &ids, &[]);
        assert!(
            problems
                .iter()
                .any(|p| matches!(p, SplitProblem::MissingAcceptance(pkg) if pkg == "pkg-spec")),
            "缺可独立验收要拒"
        );
        assert!(
            problems
                .iter()
                .any(|p| matches!(p, SplitProblem::MissingBehaviour(pkg) if pkg == "pkg-split")),
            "缺可观察行为要拒"
        );
    }

    #[test]
    fn unresolved_mappings_block_split() {
        let (plan, specs, ids) = base_plan();
        let unresolved = vec![TaskMapping {
            task_id: "task-x".to_string(),
            source_path: "tasks.md".to_string(),
            section: "1. 第一组".to_string(),
            display: Some("1.2".to_string()),
            fingerprint: "ff".to_string(),
            state: MatchState::Unresolved {
                candidate: None,
                reason: "需确认".to_string(),
            },
        }];
        let problems = check_split(&plan, &specs, &ids, &unresolved);
        assert!(
            problems
                .iter()
                .any(|p| matches!(p, SplitProblem::UnresolvedTasks(_))),
            "有未确认映射时不许据此拆包"
        );
    }

    fn ready(change: &str, requirement: &str) -> ArchiveRequest {
        ArchiveRequest {
            change_id: change.to_string(),
            requirements: vec![requirement.to_string()],
            integration_accepted: true,
            consistency: ConsistencyCheck::Passed {
                note: "规格与实现一致".to_string(),
            },
        }
    }

    #[test]
    fn two_changes_touching_one_requirement_serialize_even_without_text_conflict() {
        // 两个 Change 改同一句需求，Git 文本完全可以无冲突——但语义上必须串行。
        let requests = vec![
            ready("change-a", "specs/x/spec.md/稳定任务身份"),
            ready("change-b", "specs/x/spec.md/稳定任务身份"),
        ];
        let plan = plan_archive(&requests).unwrap();
        assert!(!plan.parallel_safe, "共享 requirement 时必须串行");
        assert_eq!(plan.shared_requirements.len(), 1);
        assert_eq!(plan.steps.len(), 2);
        assert_eq!(plan.steps[1].waits_for, vec!["change-a".to_string()]);
    }

    #[test]
    fn independent_changes_may_archive_in_parallel() {
        let requests = vec![
            ready("change-a", "specs/x/spec.md/需求甲"),
            ready("change-b", "specs/y/spec.md/需求乙"),
        ];
        let plan = plan_archive(&requests).unwrap();
        assert!(plan.parallel_safe);
        assert!(plan.shared_requirements.is_empty());
        assert!(plan.steps.iter().all(|step| step.waits_for.is_empty()));
    }

    #[test]
    fn archive_requires_integration_acceptance_and_consistency_check() {
        let mut request = ready("change-a", "specs/x/需求甲");
        request.integration_accepted = false;
        assert!(matches!(
            plan_archive(&[request]).unwrap_err(),
            OpenspecError::NotReadyToArchive { .. }
        ));

        let mut request = ready("change-a", "specs/x/需求甲");
        request.consistency = ConsistencyCheck::NotRun;
        assert!(matches!(
            plan_archive(&[request]).unwrap_err(),
            OpenspecError::NotReadyToArchive { .. }
        ));

        let mut request = ready("change-a", "specs/x/需求甲");
        request.consistency = ConsistencyCheck::Failed("需求 3 无实现".to_string());
        let err = plan_archive(&[request]).unwrap_err().to_string();
        assert!(err.contains("一致性检查失败"), "实际 {err}");
    }

    #[test]
    fn tasks_without_display_numbers_still_get_stable_ids() {
        let tasks = parse_tasks("- [ ] 没有编号的任务\n- [ ] 另一条\n");
        assert_eq!(tasks.len(), 2);
        assert!(tasks[0].display.is_none());
        assert_eq!(tasks[0].text, "没有编号的任务");
        let first = remap_tasks(&[], "tasks.md", &tasks);
        let again = remap_tasks(
            &first.mappings,
            "tasks.md",
            &parse_tasks("- [ ] 另一条\n- [ ] 没有编号的任务\n"),
        );
        let before: BTreeSet<String> = first.mappings.iter().map(|m| m.task_id.clone()).collect();
        let after: BTreeSet<String> = again.mappings.iter().map(|m| m.task_id.clone()).collect();
        assert_eq!(before, after);
    }
}
