# ADR-0007 主体与目标判定的唯一入口与装配边界

- 状态：`proposed`（内容待项目负责人签署；本 ADR 不改任何代码路径）
- 日期：2026-11-22
- 决策者：待签（建议人：架构评审）
- 关联：V2.9.9 技术规格 §8.1；产品方案 §34.14 / §34.15；`openspec/changes/p8-delivery-tracking`
- 取代：无

## 场景

§11 的两条硬要求是「读取按授权目标返回」「对调用者不可见的客户不出现在 targets 里」，
`delivery_api` 六条路由的 `note` 都写着「§34.12 的主体与目标判定未实现」，拒绝码统一
`BOD_API_DENIED`。

但仓库盘点结果与这句话**不完全一致**，必须先分清：

| 事实 | 证据 |
|---|---|
| 判定**已经写好**：服务端过滤、可见≠可做、订阅续传各自有函数 | `authorization::filter_records` / `access_record` / `check_subscription`（P5 2.2，AZ01/AZ06 用例） |
| 判定**在生产零调用** | `authorize_stage` 的调用点全在 `tests/` 与 `audit_log` 用例内；`StageFacts` 4 处构造全在测试 |
| 装配来源**不存在** | 没有任何从 `SqliteStore` 读出 grants / members / customer_bindings 的函数 |
| 目标侧字段**已经就位** | `delivery_targets.customer_id`（016 迁移）与 `ScopedRecord::for_customer` 语义一致 |

所以缺口是**接线**，不是**判定**。这决定了本 ADR 只讨论三件事：入口唯一、装配边界、拒绝码分级。

## 决策

1. **`authorization` 是唯一入口，不新建访问控制模块，不扩 `StageAction`。**
   目标过滤走 `access_record(facts, actor, record, action, target, at)`，它一次给出
   `Invisible` 或 `Allowed(StageDecision)`——「不可见」与「可见但缺权」在类型上就分开，
   不允许调用方自己拼「先查再看」。

2. **新增装配端口 `StageFactsPort`（bod-app 内的纯函数 + 只读查询），负责把库里的
   grants / assignments / members / customer_bindings / revoked_at_revision 装进 `StageFacts`。**
   装配是**读**，不写库、不派发；端口只暴露「按主体 + 项目取事实」一个动作。

3. **目标映射固定为一条规则**：`DeliveryTarget` → `ScopedRecord { scope: 跟踪项 scope,
   customer_id: Some(target.customer_id) }`。跟踪项本身按 `scope`（project）判定，
   目标按 `customer_id` 判定。客户协作者只看自己客户的目标，父项目与兄弟目标一律不可见。

4. **拒绝码分三档，替换现在「一律 `BOD_API_DENIED`」**：

   | 情形 | 码 | 为什么不合并 |
   |---|---|---|
   | 未通过会话认证 | `BOD_API_DENIED` | 与主体无关，重试无用 |
   | 对象不可见 | `BOD_NOT_FOUND` | 用 403 会泄漏「这条存在」，AZ01 要求不产生差异信号 |
   | 可见但缺动作权 | `BOD_SCOPE_DENIED` | 调用方应改请求主体或申请授权，处置方式不同 |

5. **顺序：先 `GET /delivery-tracking/{id}`（只读），后写端点。** 读路径是目标过滤的最小
   完整用例；先做它，才能证明写入口的授权语义没有被顺手做松。

## 理由

- **不新建模块**：`filter_records` 与 `access_record` 已经有 AZ01/AZ06 的命名用例，
  再写一套「交付专用的可见性判定」会出现第二个真相来源，且两套判定必然在某次改动后分叉。
- **装配独立成端口**：`StageFacts` 是值类型，构造它需要读 5 组事实。把读取塞进
  `authorization` 会让纯判定模块依赖 `SqliteStore`，破坏「入口不访问 DB」的既有约束
  （`authorization` 模块头第一句）。
- **拒绝码分级**：合并成 403 是最省事的写法，也是 AZ01「不泄漏存在性」最容易被破的地方。

## 代价

- 每个 `/api/v1` 请求多一次事实装配（5 组小查询）。单机 SQLite 下可接受，但必须按请求缓存，
  不能在每个目标上重查一遍——目标数量增长时这是 N+1。
- `StageFacts` 目前是 `Vec` 线性查找；`customer_bindings` 超过数百条后需要索引化结构。
- 三档拒绝码是对外行为变化，客户端错误处理要同步（当前六条路由全 `planned`，改动成本最低的时机就是现在）。

## 撤销条件（可机械判断）

1. 出现**跨 Company 查询**（多 Company 单宿主），或远程节点需要自带判定——
   此时 `StageFactsPort` 必须换成远程授权服务，本 ADR 重评；
2. `customer_bindings` 单 Company 超过 **1,000** 条且装配 p99 > 5 ms——需要索引化，重评数据结构；
3. 需要「同一对象对不同主体给出不同**字段**投影」（不只是可见/不可见）——这超出 `ScopedRecord`
   的表达力，需要新的投影层，重评第 3 条。

## 代码体现（落地时才产生）

- 已有：`crates/bod-app/src/authorization.rs`（`filter_records` / `access_record` / `ScopedRecord`）。
- 待建：`StageFactsPort` 装配函数、`delivery_api` 的 `Principal` 传参、三档拒绝码常量。
- 当前 `delivery_api` 的 `note` 与 `DENIED_CODE` 在落地时必须同步改写，测试
  `planned_routes_point_at_the_unimplemented_handler` 会挡住「悄悄当已实现」。
