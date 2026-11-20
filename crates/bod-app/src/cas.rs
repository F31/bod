//! P0 D5 内容寻址存储（任务 6.1，§33.5、invariant_i_07）。
//!
//! 顺序强制：**先写临时文件 → 计算 digest → 原子转正 → 之后才允许在事务中登记引用**。
//! 任何一步失败或崩溃都只留下孤儿文件，不会出现"登记了引用但字节没落盘"。
//!
//! GC 只扫描本 CAS 根目录下的受管文件名；外部权威存储（Git、制品仓库、文档系统）
//! 的对象一律不在扫描范围内，因此不可能被本地 GC 删除。

use crate::domain::artifact::Digest;
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// 临时文件后缀：受管但未转正，超过宽限期即视为崩溃孤儿。
const TEMP_SUFFIX: &str = ".part";

#[derive(Debug)]
pub enum CasError {
    Io(std::io::Error),
    /// 内容与声明 digest 不一致：拒绝把错误字节当成该对象。
    DigestMismatch {
        expected: String,
        actual: String,
    },
    /// 目标路径不在本 CAS 管理范围内。
    OutsideRoot(PathBuf),
    InvalidDigest(String),
}

impl std::fmt::Display for CasError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "CAS IO 失败: {err}"),
            Self::DigestMismatch { expected, actual } => {
                write!(f, "CAS 摘要不匹配：期望 {expected}，实际 {actual}")
            }
            Self::OutsideRoot(path) => {
                write!(f, "拒绝操作 CAS 根目录之外的路径: {}", path.display())
            }
            Self::InvalidDigest(value) => write!(f, "非法摘要: {value}"),
        }
    }
}

impl std::error::Error for CasError {}

impl From<std::io::Error> for CasError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

/// 已转正的对象描述：digest / 大小 / MIME 与实际路径。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CasObject {
    pub digest: Digest,
    pub size_bytes: i64,
    pub mime: String,
    pub path: PathBuf,
}

impl CasObject {
    /// 登记进 DB 时的 storage_ref 取值：直接用 digest 十六进制正文。
    pub fn storage_ref(&self) -> String {
        self.digest.value.clone()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrphanReason {
    /// 已转正但没有任何修订引用（登记前崩溃）。
    UnreferencedObject,
    /// 未转正的临时文件（写入或转正前崩溃）。
    StaleTempFile,
}

impl OrphanReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::UnreferencedObject => "unreferenced_object",
            Self::StaleTempFile => "stale_temp_file",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanObject {
    /// 临时文件没有可信 digest，因此为 None。
    pub digest: Option<String>,
    pub path: PathBuf,
    pub size_bytes: u64,
    pub reason: OrphanReason,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GcPlan {
    pub orphans: Vec<OrphanObject>,
    /// 有引用的对象数（保留）。
    pub retained: usize,
    /// 外部权威存储引用数：显式计入报告，且永不删除。
    pub external_refs: usize,
    /// 未超过宽限期、暂不判定的临时文件数。
    pub within_grace: usize,
}

impl GcPlan {
    pub fn total_orphan_bytes(&self) -> u64 {
        self.orphans.iter().map(|orphan| orphan.size_bytes).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.orphans.is_empty()
    }

    pub fn summary(&self) -> String {
        format!(
            "孤儿 {} 个（{} 字节），保留 {} 个，外部引用 {} 个不参与本地 GC，宽限期内 {} 个",
            self.orphans.len(),
            self.total_orphan_bytes(),
            self.retained,
            self.external_refs,
            self.within_grace
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GcApplied {
    pub deleted: usize,
    pub bytes: u64,
}

pub struct CasStore {
    root: PathBuf,
}

impl CasStore {
    /// 根目录下分 `objects/`（已转正）与 `tmp/`（写入中）。
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, CasError> {
        let root = root.into();
        fs::create_dir_all(root.join("objects"))?;
        fs::create_dir_all(root.join("tmp"))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 两字符前缀分桶，避免单目录堆积过多文件。
    pub fn object_path(&self, digest: &Digest) -> PathBuf {
        let (prefix, _) = digest.value.split_at(2);
        self.root.join("objects").join(prefix).join(&digest.value)
    }

    pub fn contains(&self, digest: &Digest) -> bool {
        self.object_path(digest).is_file()
    }

    /// 已转正对象数量（备份报告与 GC 统计用）。
    pub fn object_count(&self) -> usize {
        let objects = self.root.join("objects");
        let Ok(dirs) = fs::read_dir(&objects) else {
            return 0;
        };
        dirs.filter_map(Result::ok)
            .filter(|entry| entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false))
            .filter_map(|entry| fs::read_dir(entry.path()).ok())
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|entry| {
                        is_digest_name(&entry.file_name().to_string_lossy())
                            && entry
                                .file_type()
                                .map(|kind| kind.is_file())
                                .unwrap_or(false)
                    })
                    .count()
            })
            .sum()
    }

    /// 先落临时文件并 fsync，再按内容摘要原子转正。
    /// 返回时字节已持久化，调用方才可以开启事务登记引用。
    pub fn stage(&self, bytes: &[u8], mime: &str) -> Result<CasObject, CasError> {
        let digest = Digest::of_bytes(bytes);
        let target = self.object_path(&digest);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        if target.is_file() {
            // 内容寻址：同内容重复写入是幂等的，不产生第二个对象。
            let existing = fs::metadata(&target)?;
            if existing.len() != bytes.len() as u64 {
                return Err(CasError::DigestMismatch {
                    expected: digest.as_string(),
                    actual: format!("已存在对象长度 {}", existing.len()),
                });
            }
            return Ok(CasObject {
                digest,
                size_bytes: bytes.len() as i64,
                mime: mime.to_string(),
                path: target,
            });
        }

        let temp = self
            .root
            .join("tmp")
            .join(format!("{}{TEMP_SUFFIX}", digest.value));
        {
            let mut file = OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&temp)?;
            file.write_all(bytes)?;
            file.flush()?;
            // 字节真正落盘后才转正；崩溃只会留下临时文件。
            file.sync_all()?;
        }

        // 转正前再核对一次临时文件内容，防止写入期间被替换。
        let written = fs::read(&temp)?;
        let actual = crate::domain::artifact::sha256_hex(&written);
        if actual != digest.value {
            let _ = fs::remove_file(&temp);
            return Err(CasError::DigestMismatch {
                expected: digest.as_string(),
                actual: format!("sha256:{actual}"),
            });
        }

        match fs::rename(&temp, &target) {
            Ok(()) => {
                sync_dir(target.parent());
            }
            Err(err) if target.is_file() => {
                // 并发写入同一内容：对方已转正，丢弃自己的临时文件即可。
                let _ = fs::remove_file(&temp);
                let _ = err;
            }
            Err(err) => return Err(CasError::Io(err)),
        }

        Ok(CasObject {
            digest,
            size_bytes: bytes.len() as i64,
            mime: mime.to_string(),
            path: target,
        })
    }

    /// 读取并复核摘要：字节被篡改时不返回内容。
    pub fn read(&self, digest: &Digest) -> Result<Vec<u8>, CasError> {
        if !digest.is_usable() {
            return Err(CasError::InvalidDigest(digest.as_string()));
        }
        let path = self.object_path(digest);
        self.ensure_managed(&path)?;
        let mut buffer = Vec::new();
        File::open(&path)?.read_to_end(&mut buffer)?;
        let actual = crate::domain::artifact::sha256_hex(&buffer);
        if actual != digest.value {
            return Err(CasError::DigestMismatch {
                expected: digest.as_string(),
                actual: format!("sha256:{actual}"),
            });
        }
        Ok(buffer)
    }

    /// 扫描：已转正但无引用的对象，以及超过宽限期的临时文件。
    /// `referenced` 只应包含本 CAS 的 digest；外部引用通过 `external_refs` 计数报告。
    pub fn scan(
        &self,
        referenced: &HashSet<String>,
        external_refs: usize,
        now_ms: i64,
        grace_ms: i64,
    ) -> Result<GcPlan, CasError> {
        let mut plan = GcPlan {
            external_refs,
            ..Default::default()
        };

        let objects = self.root.join("objects");
        if objects.is_dir() {
            for dir in fs::read_dir(&objects)? {
                let dir = dir?;
                if !dir.file_type()?.is_dir() {
                    continue;
                }
                for entry in fs::read_dir(dir.path())? {
                    let entry = entry?;
                    let name = entry.file_name().to_string_lossy().to_string();
                    if !is_digest_name(&name) {
                        continue;
                    }
                    if referenced.contains(&name) {
                        plan.retained += 1;
                        continue;
                    }
                    let meta = entry.metadata()?;
                    if now_ms - modified_ms(&meta) < grace_ms {
                        plan.within_grace += 1;
                        continue;
                    }
                    plan.orphans.push(OrphanObject {
                        digest: Some(name),
                        path: entry.path(),
                        size_bytes: meta.len(),
                        reason: OrphanReason::UnreferencedObject,
                    });
                }
            }
        }

        let temp_dir = self.root.join("tmp");
        if temp_dir.is_dir() {
            for entry in fs::read_dir(&temp_dir)? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().to_string();
                if !name.ends_with(TEMP_SUFFIX) {
                    continue;
                }
                let meta = entry.metadata()?;
                if now_ms - modified_ms(&meta) < grace_ms {
                    plan.within_grace += 1;
                    continue;
                }
                plan.orphans.push(OrphanObject {
                    digest: None,
                    path: entry.path(),
                    size_bytes: meta.len(),
                    reason: OrphanReason::StaleTempFile,
                });
            }
        }

        plan.orphans
            .sort_by(|left, right| left.path.cmp(&right.path));
        Ok(plan)
    }

    /// 分批删除：每次最多 `limit` 个，便于审计与中断后继续。
    /// 只删除 `scan` 报告且位于本 CAS 根目录内的文件。
    pub fn gc(&self, plan: &GcPlan, limit: usize) -> Result<GcApplied, CasError> {
        let mut applied = GcApplied {
            deleted: 0,
            bytes: 0,
        };
        for orphan in plan.orphans.iter().take(limit) {
            self.ensure_managed(&orphan.path)?;
            match fs::remove_file(&orphan.path) {
                Ok(()) => {
                    applied.deleted += 1;
                    applied.bytes += orphan.size_bytes;
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(CasError::Io(err)),
            }
        }
        Ok(applied)
    }

    /// 只允许操作根目录下由本 CAS 命名的文件，防止 plan 被伪造后删除外部内容。
    fn ensure_managed(&self, path: &Path) -> Result<(), CasError> {
        let canonical_root = self.root.canonicalize()?;
        let parent = path
            .parent()
            .ok_or_else(|| CasError::OutsideRoot(path.to_path_buf()))?;
        let canonical_parent = parent.canonicalize()?;
        if !canonical_parent.starts_with(&canonical_root) {
            return Err(CasError::OutsideRoot(path.to_path_buf()));
        }
        let name = path
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_default();
        if !is_digest_name(&name) && !name.ends_with(TEMP_SUFFIX) {
            return Err(CasError::OutsideRoot(path.to_path_buf()));
        }
        Ok(())
    }
}

fn is_digest_name(name: &str) -> bool {
    name.len() == 64 && name.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn modified_ms(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_else(now_ms)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(unix)]
fn sync_dir(path: Option<&Path>) {
    if let Some(path) = path
        && let Ok(dir) = File::open(path)
    {
        let _ = dir.sync_all();
    }
}

#[cfg(not(unix))]
fn sync_dir(_path: Option<&Path>) {
    // Windows 不支持对目录 fsync；rename 后以文件级 sync 为准。
}

/// 仅用于测试：把文件修改时间推到宽限期之外，避免真实等待。
#[cfg(test)]
fn age_file(path: &Path) {
    let file = OpenOptions::new()
        .write(true)
        .open(path)
        .expect("可打开文件");
    let past = SystemTime::now() - std::time::Duration::from_secs(86_400);
    file.set_modified(past).expect("可设置 mtime");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::artifact::DigestAlgorithm;
    use std::sync::atomic::{AtomicU32, Ordering};

    static SEQ: AtomicU32 = AtomicU32::new(0);

    fn temp_root() -> PathBuf {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "bod-cas-{}-{}-{}",
            std::process::id(),
            seq,
            now_ms()
        ))
    }

    fn store() -> (CasStore, PathBuf) {
        let root = temp_root();
        let store = CasStore::open(&root).expect("CAS 根目录可建");
        (store, root)
    }

    #[test]
    fn stage_is_content_addressed_and_idempotent() {
        let (cas, _root) = store();
        let first = cas.stage(b"hello world", "text/plain").expect("写入成功");
        assert_eq!(first.digest, Digest::of_bytes(b"hello world"));
        assert_eq!(first.size_bytes, 11);
        assert!(first.path.is_file());
        assert!(cas.contains(&first.digest));

        let second = cas
            .stage(b"hello world", "text/plain")
            .expect("重复写入幂等");
        assert_eq!(first.path, second.path);
        assert_eq!(
            fs::read_dir(cas.root().join("tmp")).unwrap().count(),
            0,
            "转正后不留临时文件"
        );
    }

    #[test]
    fn read_verifies_digest_and_rejects_tampering() {
        let (cas, _root) = store();
        let object = cas.stage(b"trusted bytes", "text/plain").expect("写入");
        assert_eq!(cas.read(&object.digest).unwrap(), b"trusted bytes");

        fs::write(&object.path, b"tampered").expect("模拟磁盘篡改");
        let err = cas.read(&object.digest).expect_err("摘要不符必须拒绝");
        assert!(matches!(err, CasError::DigestMismatch { .. }));
    }

    mod invariant_i_07 {
        use super::*;

        #[test]
        fn crash_between_rename_and_register_is_reported_as_orphan() {
            let (cas, _root) = store();
            // 模拟：字节已转正，但宿主在事务登记前崩溃（没有任何引用）。
            let object = cas.stage(b"unregistered report", "text/report").unwrap();
            age_file(&object.path);

            let plan = cas
                .scan(&HashSet::new(), 0, now_ms(), 3_600_000)
                .expect("扫描成功");
            assert_eq!(plan.orphans.len(), 1);
            assert_eq!(
                plan.orphans[0].digest.as_deref(),
                Some(object.digest.value.as_str())
            );
            assert_eq!(plan.orphans[0].reason, OrphanReason::UnreferencedObject);
            assert!(plan.summary().contains("孤儿 1 个"));
        }

        #[test]
        fn registered_reference_is_retained() {
            let (cas, _root) = store();
            let object = cas.stage(b"registered", "text/plain").unwrap();
            age_file(&object.path);

            let mut referenced = HashSet::new();
            referenced.insert(object.digest.value.clone());
            let plan = cas.scan(&referenced, 0, now_ms(), 3_600_000).unwrap();
            assert!(plan.orphans.is_empty());
            assert_eq!(plan.retained, 1);
        }

        #[test]
        fn external_objects_are_never_deleted_or_counted_as_local_gc_targets() {
            let (cas, root) = store();
            // 外部权威存储内容（Git 工作区/制品仓库）放在 CAS 根之外的独立目录。
            let external = root.join("external-repo");
            fs::create_dir_all(&external).unwrap();
            let external_file = external.join("design.md");
            fs::write(&external_file, b"external authority").unwrap();
            age_file(&external_file);

            let plan = cas
                .scan(&HashSet::new(), 3, now_ms(), 3_600_000)
                .expect("扫描成功");
            assert_eq!(plan.external_refs, 3);
            assert!(
                plan.orphans.is_empty(),
                "外部目录不在扫描范围内，不得被判为孤儿"
            );

            let applied = cas.gc(&plan, 10).unwrap();
            assert_eq!(applied.deleted, 0);
            assert!(external_file.is_file(), "外部仓库对象必须保留");
        }

        #[test]
        fn unmanaged_path_cannot_be_deleted_even_if_planned() {
            let (cas, root) = store();
            let outside = root.join("external-repo").join("keep.bin");
            fs::create_dir_all(outside.parent().unwrap()).unwrap();
            fs::write(&outside, b"keep").unwrap();

            let forged = GcPlan {
                orphans: vec![OrphanObject {
                    digest: None,
                    path: outside.clone(),
                    size_bytes: 4,
                    reason: OrphanReason::UnreferencedObject,
                }],
                ..Default::default()
            };
            assert!(matches!(
                cas.gc(&forged, 10).unwrap_err(),
                CasError::OutsideRoot(_)
            ));
            assert!(outside.is_file());
        }
    }

    #[test]
    fn stale_temp_file_is_orphan_and_fresh_temp_is_within_grace() {
        let (cas, root) = store();
        let stale = root
            .join("tmp")
            .join(format!("{}{TEMP_SUFFIX}", "a".repeat(64)));
        fs::write(&stale, b"partial").unwrap();
        age_file(&stale);

        let fresh = root
            .join("tmp")
            .join(format!("{}{TEMP_SUFFIX}", "b".repeat(64)));
        fs::write(&fresh, b"writing").unwrap();

        let plan = cas.scan(&HashSet::new(), 0, now_ms(), 3_600_000).unwrap();
        assert_eq!(plan.orphans.len(), 1);
        assert_eq!(plan.orphans[0].reason, OrphanReason::StaleTempFile);
        assert_eq!(plan.within_grace, 1);
    }

    #[test]
    fn gc_respects_batch_limit() {
        let (cas, _root) = store();
        let mut paths = Vec::new();
        for index in 0..3 {
            let object = cas
                .stage(format!("object-{index}").as_bytes(), "text/plain")
                .unwrap();
            age_file(&object.path);
            paths.push(object.path);
        }
        let plan = cas.scan(&HashSet::new(), 0, now_ms(), 3_600_000).unwrap();
        assert_eq!(plan.orphans.len(), 3);

        let first = cas.gc(&plan, 2).unwrap();
        assert_eq!(first.deleted, 2);
        assert_eq!(plan.orphans.iter().filter(|o| o.path.is_file()).count(), 1);
    }

    #[test]
    fn empty_content_has_stable_digest_and_path() {
        let (cas, _root) = store();
        let object = cas.stage(b"", "application/octet-stream").unwrap();
        assert_eq!(
            object.digest.value,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(object.size_bytes, 0);
        assert_eq!(cas.read(&object.digest).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn invalid_digest_is_rejected_before_touching_disk() {
        let (cas, _root) = store();
        let bad = Digest {
            algorithm: DigestAlgorithm::Sha256,
            value: "zz".to_string(),
        };
        assert!(matches!(
            cas.read(&bad).unwrap_err(),
            CasError::InvalidDigest(_)
        ));
    }
}
