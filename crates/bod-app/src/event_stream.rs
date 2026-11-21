//! HTTP/SSE 基线订阅：持久游标语义、慢订阅释放与快照基准（P6 2.6 / RM06）。
//!
//! 传输层本身只做三件事：把**已持久**的游标翻译成补齐计划、把事件编码成 SSE 帧、
//! 在拿不到增量时**如实**要求重订阅。它不猜"客户端大概收到了哪"：
//!
//! - 游标优先级：`Last-Event-ID` 头 > `?cursor=` > 已持久游标 > 从头；
//! - 游标早于保留窗口 → 用带基准序列的快照，不做增量拼接；
//! - 连快照都没有 / 游标无法解析 / 增量出现空洞 → `event: resync`，绝不静默跳到最新；
//! - 写不出去（慢订阅/断连）→ 关闭连接并保留游标，由客户端带 `Last-Event-ID` 重连补拉。
//!
//! 为什么"要求重订阅"而不是"从最新继续"：后者会让客户端以为连续，实际中间缺一段，
//! 控制命令会基于错误状态给出"成功"回执——这正是 RM06 要禁止的假象。

use std::sync::Mutex;

/// SSE 响应内容类型。
pub const SSE_CONTENT_TYPE: &str = "text/event-stream";
/// 建议客户端重连间隔（毫秒）；只在 `resync` 帧里出现。
pub const DEFAULT_RETRY_MS: u64 = 2_000;
/// 单次补齐最多推送多少条，避免一次订阅把积压全塞进内存。
pub const DEFAULT_BACKLOG_LIMIT: usize = 256;

/// 一条持久事件。`sequence` 由事件源单调分配，客户端游标就是它。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamEvent {
    pub sequence: u64,
    /// SSE `event:` 名，例如 `work.queue`、`host.ready`。
    pub kind: String,
    /// 已经是 JSON 文本；传输层不再解析内容，避免两处 schema 漂移。
    pub data: String,
}

impl StreamEvent {
    pub fn new(sequence: u64, kind: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            sequence,
            kind: kind.into(),
            data: data.into(),
        }
    }
}

/// 事件源：传输层只依赖这组事实，不依赖具体存储。
pub trait EventFeed: Send + Sync {
    /// 当前最大序列；没有事件时为 0。
    fn head(&self) -> u64;

    /// 保留窗口下界（含）。`None` 表示没有保留声明，只能按增量读；
    /// 读回来有空洞时仍按 `resync` 处理。
    fn retained_floor(&self) -> Option<u64>;

    /// 读 `from..=head` 的增量，最多 `limit` 条；**必须**按序列升序返回。
    fn read_from(&self, from: u64, limit: usize) -> Vec<StreamEvent>;

    /// 快照基准：`(baseline, 事件)`。`baseline` 是快照对应的序列，
    /// 客户端应以它为新的游标起点。
    fn snapshot(&self) -> Option<(u64, Vec<StreamEvent>)>;
}

/// 内存事件源：宿主启动期与测试用，容量有限，超出即抬高保留窗口下界。
#[derive(Debug, Default)]
pub struct MemoryFeed {
    events: Mutex<Vec<StreamEvent>>,
    capacity: usize,
}

impl MemoryFeed {
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            events: Mutex::new(Vec::new()),
            capacity: capacity.max(1),
        }
    }

    /// 追加一条事件，返回分配到的序列。
    pub fn publish(&self, kind: impl Into<String>, data: impl Into<String>) -> u64 {
        let mut events = self.events.lock().expect("事件源锁未中毒");
        let sequence = events.last().map(|event| event.sequence + 1).unwrap_or(1);
        events.push(StreamEvent::new(sequence, kind, data));
        if events.len() > self.capacity {
            let drop = events.len() - self.capacity;
            events.drain(0..drop);
        }
        sequence
    }

    pub fn len(&self) -> usize {
        self.events.lock().expect("事件源锁未中毒").len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl EventFeed for MemoryFeed {
    fn head(&self) -> u64 {
        self.events
            .lock()
            .expect("事件源锁未中毒")
            .last()
            .map(|event| event.sequence)
            .unwrap_or(0)
    }

    fn retained_floor(&self) -> Option<u64> {
        self.events
            .lock()
            .expect("事件源锁未中毒")
            .first()
            .map(|event| event.sequence)
    }

    fn read_from(&self, from: u64, limit: usize) -> Vec<StreamEvent> {
        self.events
            .lock()
            .expect("事件源锁未中毒")
            .iter()
            .filter(|event| event.sequence >= from)
            .take(limit)
            .cloned()
            .collect()
    }

    fn snapshot(&self) -> Option<(u64, Vec<StreamEvent>)> {
        let events = self.events.lock().expect("事件源锁未中毒").clone();
        let baseline = events.last().map(|event| event.sequence)?;
        Some((baseline, events))
    }
}

/// 订阅请求：游标来自哪里。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CursorRequest {
    /// 从头开始（首次订阅）。
    FromStart,
    /// 客户端声明的游标。
    After(u64),
    /// 客户端给了游标，但没法解析成序列号。
    Unparsable(String),
}

impl CursorRequest {
    /// 解析请求头的 `Last-Event-ID` 或查询参数 `cursor`，并让前者优先。
    ///
    /// 都不存在时用已持久游标；连持久游标都没有才算首次订阅。
    pub fn resolve(
        last_event_id: Option<&str>,
        query_cursor: Option<&str>,
        persisted: Option<u64>,
    ) -> Self {
        if let Some(value) = last_event_id {
            return Self::parse(value);
        }
        if let Some(value) = query_cursor {
            return Self::parse(value);
        }
        match persisted {
            Some(cursor) => Self::After(cursor),
            None => Self::FromStart,
        }
    }

    fn parse(value: &str) -> Self {
        let trimmed = value.trim();
        match trimmed.parse::<u64>() {
            Ok(cursor) => Self::After(cursor),
            Err(_) => Self::Unparsable(trimmed.to_string()),
        }
    }
}

/// 补齐计划：传输层据此决定先发什么、发完是否关连接。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamPlan {
    /// 从 `from` 到 `to` 的增量（可能为空，靠心跳保活）。
    Delta {
        from: u64,
        to: u64,
        events: Vec<StreamEvent>,
    },
    /// 游标早于保留窗口：先发快照基准，再补基准之后的增量。
    Snapshot {
        baseline: u64,
        to: u64,
        events: Vec<StreamEvent>,
    },
    /// 无法保证连续：要求客户端整体重订阅。
    Resync { reason: &'static str },
}

impl StreamPlan {
    /// 客户端在此计划后应持久化的游标。`Resync` 不变更游标（它还没拿到连续数据）。
    pub fn next_cursor(&self) -> Option<u64> {
        match self {
            Self::Delta { to, .. } | Self::Snapshot { to, .. } => Some(*to),
            Self::Resync { .. } => None,
        }
    }

    pub fn is_resync(&self) -> bool {
        matches!(self, Self::Resync { .. })
    }
}

/// 计算补齐计划。判断只看游标与保留窗口，不看连接"看起来是否正常"。
///
/// `subscriber_released` 表示该订阅缓冲已被释放（慢订阅），此时不能再发增量，
/// 只能快照或重订阅。
pub fn plan_stream(
    feed: &dyn EventFeed,
    cursor: &CursorRequest,
    subscriber_released: bool,
    backlog_limit: usize,
) -> StreamPlan {
    let head = feed.head();
    let from = match cursor {
        CursorRequest::Unparsable(_) => {
            return StreamPlan::Resync {
                reason: "游标无法解析为序列号",
            };
        }
        CursorRequest::FromStart => 1,
        CursorRequest::After(cursor) => cursor.saturating_add(1),
    };

    if let CursorRequest::After(cursor) = cursor
        && let Some(floor) = feed.retained_floor()
        && *cursor + 1 < floor
    {
        return snapshot_or_resync(feed, head, backlog_limit, "游标早于保留窗口");
    }
    if subscriber_released {
        return snapshot_or_resync(feed, head, backlog_limit, "订阅缓冲已释放");
    }
    if from > head {
        // 已经追平：没有增量，连接靠心跳保活。
        return StreamPlan::Delta {
            from,
            to: head,
            events: Vec::new(),
        };
    }

    let events = feed.read_from(from, backlog_limit);
    let continuous = match events.first() {
        Some(event) => event.sequence == from,
        // 声称还有事件却读不到，说明保留窗口与事件源不一致：不能当"没有增量"。
        None => false,
    };
    if !continuous {
        return StreamPlan::Resync {
            reason: "增量出现空洞，无法保证连续",
        };
    }
    StreamPlan::Delta {
        from,
        to: events.last().map(|event| event.sequence).unwrap_or(head),
        events,
    }
}

fn snapshot_or_resync(
    feed: &dyn EventFeed,
    head: u64,
    backlog_limit: usize,
    reason: &'static str,
) -> StreamPlan {
    match feed.snapshot() {
        Some((baseline, events)) => StreamPlan::Snapshot {
            baseline,
            to: head,
            events: events.into_iter().take(backlog_limit).collect(),
        },
        None => StreamPlan::Resync { reason },
    }
}

/// SSE 帧。编码细节集中在这里，避免各处手拼 `data:` 漏掉换行转义。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamFrame {
    /// 一条事件：带 `id:` 供客户端回传 `Last-Event-ID`。
    Event(StreamEvent),
    /// 快照基准：客户端应把游标重置为 `baseline`。
    Snapshot { baseline: u64 },
    /// 必须整体重订阅；带 `retry:` 建议间隔。
    Resync { reason: &'static str, retry_ms: u64 },
    /// 保活注释行，不产生事件也不移动游标。
    Heartbeat,
}

impl StreamFrame {
    /// 编码成 SSE 文本（以空行结束）。
    pub fn encode(&self) -> String {
        match self {
            Self::Event(event) => format!(
                "id: {}\nevent: {}\ndata: {}\n\n",
                event.sequence,
                sanitize_field(&event.kind),
                sanitize_data(&event.data)
            ),
            Self::Snapshot { baseline } => {
                format!("event: snapshot\ndata: {{\"baseline\":{baseline}}}\n\n")
            }
            Self::Resync { reason, retry_ms } => format!(
                "event: resync\nretry: {retry_ms}\ndata: {{\"reason\":\"{}\"}}\n\n",
                sanitize_data(reason)
            ),
            Self::Heartbeat => ": heartbeat\n\n".to_string(),
        }
    }
}

/// `event:` 名只允许 `[A-Za-z0-9._-]`：事件名是我们自己定义的标识，
/// 不是用户内容。其他字符一律替换为 `_`，避免换行/冒号伪造出额外字段。
fn sanitize_field(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        if ch.is_ascii_alphanumeric() || ch == '.' || ch == '_' || ch == '-' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    out
}

/// `data:` 允许多行，规范要求每行独立加前缀；这里统一压成单行 JSON，
/// 避免客户端按行拆出多余事件。
fn sanitize_data(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            '\n' | '\r' => ' ',
            other => other,
        })
        .collect()
}

/// 把一个补齐计划渲染成帧序列。
pub fn render_plan(plan: &StreamPlan, retry_ms: u64) -> Vec<StreamFrame> {
    match plan {
        StreamPlan::Delta { events, .. } => {
            let mut frames: Vec<StreamFrame> =
                events.iter().cloned().map(StreamFrame::Event).collect();
            if frames.is_empty() {
                frames.push(StreamFrame::Heartbeat);
            }
            frames
        }
        StreamPlan::Snapshot {
            baseline, events, ..
        } => {
            let mut frames = vec![StreamFrame::Snapshot {
                baseline: *baseline,
            }];
            frames.extend(events.iter().cloned().map(StreamFrame::Event));
            frames
        }
        StreamPlan::Resync { reason } => vec![StreamFrame::Resync { reason, retry_ms }],
    }
}

/// 订阅鉴权结果。传输层只认 Bearer 会话密钥，不接受查询串里的令牌
/// （查询串会进日志与浏览器历史）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamAuth {
    Authorized,
    Missing,
    WrongSecret,
    /// 用了 `?token=` 之类的地方传凭据。
    InQuery,
}

impl StreamAuth {
    pub fn is_authorized(&self) -> bool {
        matches!(self, Self::Authorized)
    }

    pub fn explain(&self) -> &'static str {
        match self {
            Self::Authorized => "已授权",
            Self::Missing => "缺少 Authorization: Bearer <session_key>",
            Self::WrongSecret => "会话密钥不匹配",
            Self::InQuery => "订阅不接受查询串令牌：会写入日志与浏览器历史",
        }
    }
}

/// 检查订阅请求的鉴权头。`token_eq` 由调用方提供（控制面已有常量时间比较）。
pub fn authorize_stream(
    authorization: Option<&str>,
    has_query_token: bool,
    expected: &str,
    token_eq: impl Fn(&str, &str) -> bool,
) -> StreamAuth {
    if has_query_token {
        return StreamAuth::InQuery;
    }
    let Some(value) = authorization else {
        return StreamAuth::Missing;
    };
    let Some(presented) = value.strip_prefix("Bearer ") else {
        return StreamAuth::Missing;
    };
    if token_eq(presented.trim(), expected) {
        StreamAuth::Authorized
    } else {
        StreamAuth::WrongSecret
    }
}

/// 订阅生命周期里的可观测量，写进日志与报告都用它，避免"感觉订阅正常"。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamStats {
    pub events_sent: usize,
    pub frames_sent: usize,
    pub resyncs: usize,
    pub last_cursor: Option<u64>,
}

impl StreamStats {
    pub fn record_frame(&mut self, frame: &StreamFrame) {
        self.frames_sent += 1;
        match frame {
            StreamFrame::Event(event) => {
                self.events_sent += 1;
                self.last_cursor = Some(event.sequence);
            }
            StreamFrame::Snapshot { baseline } => {
                self.last_cursor = Some(*baseline);
            }
            StreamFrame::Resync { .. } => {
                self.resyncs += 1;
            }
            StreamFrame::Heartbeat => {}
        }
    }

    /// 本次订阅是否给客户端提供了连续数据（可以据此持久化游标）。
    pub fn is_continuous(&self) -> bool {
        self.resyncs == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed_with(events: &[(u64, &str)]) -> MemoryFeed {
        let feed = MemoryFeed::with_capacity(64);
        for (sequence, kind) in events {
            let published = feed.publish(*kind, format!("{{\"seq\":{sequence}}}"));
            assert_eq!(published, *sequence);
        }
        feed
    }

    fn token_eq(left: &str, right: &str) -> bool {
        left == right
    }

    #[test]
    fn resume_from_last_event_id_returns_only_the_delta() {
        let feed = feed_with(&[(1, "a"), (2, "b"), (3, "c")]);
        let plan = plan_stream(
            &feed,
            &CursorRequest::After(1),
            false,
            DEFAULT_BACKLOG_LIMIT,
        );
        match plan {
            StreamPlan::Delta { from, to, events } => {
                assert_eq!((from, to), (2, 3));
                assert_eq!(
                    events.iter().map(|e| e.sequence).collect::<Vec<_>>(),
                    vec![2, 3],
                    "只补游标之后的事件，不重发已确认的 1"
                );
            }
            other => panic!("应为增量，实际 {other:?}"),
        }
        assert_eq!(
            plan_stream(&feed, &CursorRequest::After(1), false, 256).next_cursor(),
            Some(3)
        );
    }

    #[test]
    fn catching_up_sends_heartbeat_instead_of_nothing() {
        let feed = feed_with(&[(1, "a")]);
        let plan = plan_stream(&feed, &CursorRequest::After(1), false, 256);
        assert_eq!(
            plan,
            StreamPlan::Delta {
                from: 2,
                to: 1,
                events: Vec::new()
            }
        );
        let frames = render_plan(&plan, DEFAULT_RETRY_MS);
        assert_eq!(frames, vec![StreamFrame::Heartbeat], "追平后必须保活");
    }

    #[test]
    fn cursor_before_retention_uses_snapshot_baseline() {
        let feed = MemoryFeed::with_capacity(2);
        for sequence in 1..=4 {
            feed.publish("work.queue", format!("{{\"seq\":{sequence}}}"));
        }
        // 容量 2 → 只剩 3、4，保留窗口下界是 3。
        assert_eq!(feed.retained_floor(), Some(3));

        let plan = plan_stream(&feed, &CursorRequest::After(1), false, 256);
        match plan {
            StreamPlan::Snapshot {
                baseline,
                to,
                events,
            } => {
                assert_eq!(baseline, 4, "快照基准就是当前 head");
                assert_eq!(to, 4);
                assert_eq!(events.len(), 2, "快照要自带全量窗口内容");
            }
            other => panic!("应读快照基准，实际 {other:?}"),
        }
        let frames = render_plan(
            &StreamPlan::Snapshot {
                baseline: 4,
                to: 4,
                events: vec![],
            },
            DEFAULT_RETRY_MS,
        );
        assert_eq!(frames.first(), Some(&StreamFrame::Snapshot { baseline: 4 }));
    }

    #[test]
    fn expired_cursor_without_snapshot_requires_full_resync() {
        struct NoSnapshot;
        impl EventFeed for NoSnapshot {
            fn head(&self) -> u64 {
                10
            }
            fn retained_floor(&self) -> Option<u64> {
                Some(9)
            }
            fn read_from(&self, _from: u64, _limit: usize) -> Vec<StreamEvent> {
                Vec::new()
            }
            fn snapshot(&self) -> Option<(u64, Vec<StreamEvent>)> {
                None
            }
        }

        let plan = plan_stream(&NoSnapshot, &CursorRequest::After(1), false, 256);
        assert_eq!(
            plan,
            StreamPlan::Resync {
                reason: "游标早于保留窗口"
            }
        );
        assert_eq!(plan.next_cursor(), None, "重订阅前不得推进游标");
        let frames = render_plan(&plan, 1500);
        assert_eq!(frames.len(), 1);
        let encoded = frames[0].encode();
        assert!(encoded.contains("event: resync"));
        assert!(encoded.contains("retry: 1500"));
        assert!(
            !encoded.contains("id: "),
            "resync 帧不能带 id，否则客户端会误以为游标前进"
        );
    }

    #[test]
    fn unparsable_cursor_is_not_treated_as_latest() {
        let feed = feed_with(&[(1, "a"), (2, "b")]);
        let cursor = CursorRequest::resolve(Some("abc"), None, Some(1));
        assert_eq!(cursor, CursorRequest::Unparsable("abc".to_string()));
        let plan = plan_stream(&feed, &cursor, false, 256);
        assert!(
            plan.is_resync(),
            "无法解析的游标不能静默从最新开始，否则中间事件被吞"
        );
    }

    #[test]
    fn released_slow_subscriber_cannot_keep_receiving_delta() {
        let feed = feed_with(&[(1, "a"), (2, "b")]);
        let plan = plan_stream(&feed, &CursorRequest::After(0), true, 256);
        assert!(
            matches!(
                plan,
                StreamPlan::Snapshot { .. } | StreamPlan::Resync { .. }
            ),
            "缓冲已释放后只能快照或重订阅，实际 {plan:?}"
        );
    }

    #[test]
    fn gap_in_the_middle_is_reported_not_hidden() {
        struct Holey;
        impl EventFeed for Holey {
            fn head(&self) -> u64 {
                5
            }
            fn retained_floor(&self) -> Option<u64> {
                Some(1)
            }
            fn read_from(&self, _from: u64, _limit: usize) -> Vec<StreamEvent> {
                // 声称保留窗口覆盖 1..=5，但从 3 读回来的是 4：中间缺 3。
                vec![StreamEvent::new(4, "work.queue", "{}")]
            }
            fn snapshot(&self) -> Option<(u64, Vec<StreamEvent>)> {
                None
            }
        }
        let plan = plan_stream(&Holey, &CursorRequest::After(2), false, 256);
        assert_eq!(
            plan,
            StreamPlan::Resync {
                reason: "增量出现空洞，无法保证连续"
            }
        );
    }

    #[test]
    fn frames_are_well_formed_sse() {
        let frame = StreamFrame::Event(StreamEvent::new(
            7,
            "work.queue",
            "{\"id\":\"w-1\",\"multi\":\"a\\nb\"}",
        ));
        let encoded = frame.encode();
        assert!(encoded.starts_with("id: 7\n"));
        assert!(encoded.contains("event: work.queue\n"));
        assert!(encoded.contains("data: "), "必须用 data: 行承载内容");
        assert!(encoded.ends_with("\n\n"), "帧必须以空行结束");
        assert_eq!(
            encoded.matches("\n\n").count(),
            1,
            "一帧只能有一个空行分隔，否则客户端会拆成两帧"
        );

        // 事件名与内容里的换行必须被压平，否则能伪造出额外字段。
        let injected = StreamFrame::Event(StreamEvent::new(
            8,
            "evil\nevent: forged",
            "x\n\nevent: forged",
        ));
        let encoded = injected.encode();
        assert!(
            !encoded
                .lines()
                .any(|line| line.starts_with("event: forged")),
            "事件名必须按白名单净化，不能被注入字段：{encoded}"
        );
        assert!(encoded.contains("event: evil_event__forged"));
        for line in encoded.lines().filter(|line| !line.is_empty()) {
            assert!(
                line.starts_with("id: ")
                    || line.starts_with("event: ")
                    || line.starts_with("data: "),
                "帧里只允许这三种字段行：{line}"
            );
        }
    }

    #[test]
    fn heartbeat_is_a_comment_line() {
        let encoded = StreamFrame::Heartbeat.encode();
        assert_eq!(encoded, ": heartbeat\n\n");
    }

    #[test]
    fn stream_auth_accepts_only_bearer_header() {
        assert_eq!(
            authorize_stream(Some("Bearer sk-1"), false, "sk-1", token_eq),
            StreamAuth::Authorized
        );
        assert_eq!(
            authorize_stream(Some("Bearer sk-2"), false, "sk-1", token_eq),
            StreamAuth::WrongSecret
        );
        assert_eq!(
            authorize_stream(None, false, "sk-1", token_eq),
            StreamAuth::Missing
        );
        assert_eq!(
            authorize_stream(Some("Basic sk-1"), false, "sk-1", token_eq),
            StreamAuth::Missing
        );
        assert_eq!(
            authorize_stream(Some("Bearer sk-1"), true, "sk-1", token_eq),
            StreamAuth::InQuery,
            "查询串里的令牌会进日志，必须直接拒绝"
        );
        assert!(StreamAuth::InQuery.explain().contains("日志"));
    }

    #[test]
    fn cursor_request_prefers_header_then_query_then_persisted() {
        assert_eq!(
            CursorRequest::resolve(Some("5"), Some("3"), Some(1)),
            CursorRequest::After(5)
        );
        assert_eq!(
            CursorRequest::resolve(None, Some("3"), Some(1)),
            CursorRequest::After(3)
        );
        assert_eq!(
            CursorRequest::resolve(None, None, Some(1)),
            CursorRequest::After(1)
        );
        assert_eq!(
            CursorRequest::resolve(None, None, None),
            CursorRequest::FromStart
        );
    }

    #[test]
    fn stats_distinguish_continuous_delivery_from_resync() {
        let mut stats = StreamStats::default();
        for frame in render_plan(
            &StreamPlan::Delta {
                from: 1,
                to: 2,
                events: vec![
                    StreamEvent::new(1, "a", "{}"),
                    StreamEvent::new(2, "b", "{}"),
                ],
            },
            DEFAULT_RETRY_MS,
        ) {
            stats.record_frame(&frame);
        }
        assert_eq!(stats.events_sent, 2);
        assert_eq!(stats.last_cursor, Some(2));
        assert!(stats.is_continuous());

        let mut bad = StreamStats::default();
        bad.record_frame(&StreamFrame::Resync {
            reason: "游标早于保留窗口",
            retry_ms: DEFAULT_RETRY_MS,
        });
        assert_eq!(bad.resyncs, 1);
        assert!(!bad.is_continuous(), "重订阅不得被算作连续投递");
        assert_eq!(bad.last_cursor, None);
    }

    #[test]
    fn memory_feed_bounds_retention_and_reports_floor() {
        let feed = MemoryFeed::with_capacity(3);
        assert_eq!(feed.head(), 0, "空事件源 head 为 0");
        assert_eq!(feed.retained_floor(), None);
        for _ in 0..5 {
            feed.publish("work.queue", "{}");
        }
        assert_eq!(feed.head(), 5);
        assert_eq!(feed.retained_floor(), Some(3));
        assert_eq!(feed.len(), 3);
        let delta = feed.read_from(4, 10);
        assert_eq!(delta.iter().map(|e| e.sequence).collect::<Vec<_>>(), [4, 5]);
    }
}
