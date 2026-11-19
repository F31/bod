//! bod-core：纯值类型与确定性规则。
//!
//! 边界（G-1）：本 crate 不依赖 DB / Web / Tauri / Agent SDK，也不依赖 bod-app / bod / xtask。
//! P0 阶段只建立边界与最小类型；领域规则在 D1 切片按 spec delta 逐条落地。

/// 领域稳定标识的基类型。具体 ID 在 D1 按资源类型细化。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DomainId(pub String);

impl DomainId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// 乐观并发条件更新的版本号基线。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u64);

impl Version {
    pub const INITIAL: Self = Self(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn domain_id_round_trip() {
        let id = DomainId::new("company-default");
        assert_eq!(id.as_str(), "company-default");
    }

    #[test]
    fn version_initial_is_one() {
        assert_eq!(Version::INITIAL, Version(1));
    }
}
