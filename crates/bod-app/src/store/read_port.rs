//! 交付跟踪的只读端口在库侧的实现（P8 3.6 B5）。
//!
//! 为什么需要这一层：`HostServer` 的 handler 签名是 `fn(&HostServer, EndpointRequest)`，
//! 而 `SqliteStore` 里只有一个 `rusqlite::Connection`——它 `Send` 但不 `Sync`，
//! 不能直接挂进需要 `Send + Sync` 的 trait object。
//!
//! 用 `Mutex` 而不是给每个请求开一个连接：
//! - 服务的请求循环本身是单线程串行的（`HostServer::run` 只有 SSE 才 spawn），
//!   所以锁上没有争用；`Mutex` 在这里是把「不是 Sync」这件事说出来，不是性能手段。
//! - 每请求开连接会把「库文件路径」变成 HTTP 层的配置项，而它属于宿主装配。
//!
//! **锁中毒不降级**：中毒说明上一次持锁时 panic 了，连接状态不可信。这时报
//! `StageFactsError::Unreadable`（503），不返回空事实——理由与装配端口一致。

use std::sync::Mutex;

use super::SqliteStore;
use super::facts::{StageFactsError, StageFactsPort};
use crate::authorization::{Principal, StageFacts};
use crate::delivery_api::TrackingRead;
use crate::delivery_tracking::TrackingView;

/// 只读端口的库侧实现。持有 store 的所有权，靠 `Mutex` 提供 `Sync`。
pub struct StoreTrackingRead {
    store: Mutex<SqliteStore>,
}

impl StoreTrackingRead {
    pub fn new(store: SqliteStore) -> Self {
        Self {
            store: Mutex::new(store),
        }
    }

    /// 在锁内执行一次只读装配。出错一律转成 `StageFactsError`：
    /// 对调用方而言「读不出来」只有一种处置（503），分成两个类型只会让映射表重复。
    fn with_store<T>(
        &self,
        read: impl FnOnce(&SqliteStore) -> Result<T, StageFactsError>,
    ) -> Result<T, StageFactsError> {
        let guard = self.store.lock().map_err(|_| StageFactsError::Unreadable {
            group: "connection",
            detail: "只读连接互斥量中毒：上一次持锁期间发生 panic".to_string(),
        })?;
        read(&guard)
    }
}

impl TrackingRead for StoreTrackingRead {
    fn stage_facts(
        &self,
        company_id: &str,
        principal: &Principal,
    ) -> Result<StageFacts, StageFactsError> {
        self.with_store(|store| {
            StageFactsPort::new(store.read_connection()).assemble(company_id, &principal.id)
        })
    }

    fn tracking_view(
        &self,
        company_id: &str,
        tracking_id: &str,
    ) -> Result<Option<TrackingView>, StageFactsError> {
        self.with_store(|store| {
            store
                .delivery_tracking_view(company_id, tracking_id)
                .map_err(|err| StageFactsError::Unreadable {
                    group: "delivery_tracking",
                    detail: err.to_string(),
                })
        })
    }
}
