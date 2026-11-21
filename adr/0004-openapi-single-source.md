# ADR-0004 OpenAPI 单源：schemars 派生 schema + 从 `ROUTES` 投影

- 状态：`accepted`
- 日期：2026-11-21
- 决策者：项目负责人（评审后授权按优先级推进）
- 关联：评审报告 CR-004、ADR-0001（保留同步栈）、ADR-0002（契约单源不豁免）
- 取代：评审报告 §9.5 中「推荐 A：引 utoipa」的初步结论

## 场景

V2.9.6/§6 与 V2.9.8/§11 合计 10 个接口，§11 原文写明「路径在 OpenAPI 冻结时定稿」。
所以在写 §11 的第一个端点之前，必须先把「接口契约」变成机器可校验的东西，
否则 §11 的路径会在实现过程中反复改，客户端类型与文档同步只能靠人。

实现前的事实盘点（不是印象）：

| 项 | 实测 |
|---|---|
| `routes` 表 | `crates/bod-app/src/host_handshake.rs` 的 `static ROUTES`，含 method/pattern/auth/note |
| handler 签名 | `fn(&S, EndpointRequest) -> Result<Reply, RouteError>`，状态码在**运行期**决定 |
| DTO | 新增 `host_api` 前为 0 个；handler 用 `serde_json::json!` 拼响应 |
| schema 依赖 | 0（`Cargo.toml` 里无 utoipa / schemars / schemars-like） |
| 框架集成 | 无 axum / actix（ADR-0001） |

依赖可用性也逐条核过（`index.crates.io` 元数据）：

| crate | 最新版 | MSRV | 生产依赖 |
|---|---|---|---|
| utoipa | 6.0.0 | 1.88 | indexmap、serde、serde_json（`utoipa-gen` 可选，`insta` 是 dev-dependency） |
| utoipa | 5.5.0 | 1.75 | 同上 |
| schemars | 1.2.3 | 1.74 | dyn-clone、ref-cast、serde、serde_json（`derive` 特性带 schemars_derive） |

本仓两平台工具链均为 1.96.0（`cargo --version` / `rustc --version`，Windows 与 WSL 各测一次），
两者都不构成 MSRV 障碍。

## 决策

1. **schema 从类型派生**：DTO 上加 `#[derive(JsonSchema)]`（schemars 1.2.3）。
2. **文档从路由表投影**：`crates/bod-app/src/openapi.rs` 遍历 `ROUTES` 生成 OAS 3.1，
   不接收任何「路径列表」参数。
3. **契约与路由同表**：`Route` 新增 `contract: Contract`（summary / request / query / responses），
   字段无默认值——新增端点必须写明对外响应，编译期强制。
4. **401/404 由机制生成**：拒绝码来自 `Auth` 携带的码；`contract.responses` 里出现 401/404
   会直接报错。这一条把「两处各写一份拒绝码」变成写不出来的形状。
5. **文档入库并做黄金比对**：`verification/openapi.json` 入库；`openapi_contract` 测试
   逐字比对生成结果，改契约不重生成会红。

## 为什么不采用评审时推荐的 utoipa（A）

评审阶段我推荐 A，理由是「B 自研要重造 schema 语义，漏一项前端类型就错」。
实现前核实发现：**A 的收益在无框架集成时不成立，而 A 的代价是真的。**

`#[utoipa::path(get, path = "/_bod/health", responses(...))]` 挂在 handler 上，
它要求把 **method、path、状态码**再写一遍。这三项在 `ROUTES` 里已经有一份，
于是 A 的方案必然同时维护两张表，只能靠一条「集合相等」的测试兜住漂移。
而 A 真正难的部分（schema 语义）与 C 完全重合——C 用的是同一个思路，只是换成 schemars。

A 的独占收益是**框架集成与 Swagger UI**：`utoipa-axum` 能直接把路由挂进 axum Router，
`utoipa-swagger-ui` 能起交互页面。本仓既无 axum（ADR-0001），也没有人要看 UI。
换句话说，A 剩下的只有「把路径写两遍」，而这恰好是 CR-004 要消灭的东西。

评审时对 B 的担心（自研 schema 边界出错）在 C 里被避开了：schema **不是自研的**，
只是把 schemars 的输出去掉 `$schema` 后放进 `components.schemas`；自研部分只有
OAS 外层结构（paths/operations/security/responses），那部分语义少、且被结构性断言覆盖。

## 代价

- 新增一个编译期依赖（schemars + dyn-clone + ref-cast + schemars_derive）。
  按 AGENTS.md 第 8 条登记，锁版本 `1.2.3`。
- OAS 外层结构由本仓维护：`oneOf`/`anyOf`/`discriminator`/多态请求体这类表达暂不支持，
  真需要时见撤销条件。
- **未跑官方 OAS 校验器**：离线环境没有 `@redocly/cli` 之类的工具，所以「文档符合 3.1 规范」
  目前由结构性断言保证（`$ref` 全部可解析、组件无死条目、必填与类型同真实序列化一致），
  不是由官方 lint 保证。这是已知缺口，不是已完成项。
- **文档不以 HTTP 端点暴露**：`verification/openapi.json` 是构建期产物，客户端从仓库读取。
  若将来要在运行时取（例如工作台内置 API 浏览器），那是一个**新增端点**，要走一遍契约冻结，
  不能顺手用 `Reply::Json` 挂上去。
- `Route` 多了一个必填字段，13 处构造点（4 条 `ROUTES` + 9 处测试）都要改。
  已在测试里用 `route_of(...)` 收敛，新增测试不再逐字段重复。

## 机械保障（`crates/bod-app/tests/openapi_contract.rs`）

| 断言 | 失效时说明什么 |
|---|---|
| 每个 `$ref` 都能解析 | 有人给 body 起了组件名却没登记 schema |
| 每个组件都被引用 | 端点删了但 schema 留着（死组件会误导客户端） |
| 文档 (method, path) 集合 == `ROUTES` 集合 | 文档与实现漂移 |
| 401 的 `error` 例子 == `Auth` 的码 | 拒绝码写了两份且不一致 |
| 每个操作都有 404 | 忘了分发层的真实行为 |
| `security` 声明与 `Auth` 档位一致 | 把「handler 内校验」写成「标准 Bearer」 |
| schema 字段/必填/类型 == 真实序列化结果 | serde 与 schema 之间的漂移（含 `Option` + `skip_serializing_if`） |
| 入库文档 == 生成结果，且改坏副本必须不等 | 黄金比对不能是空转 |

`document()` 的错误分支（重复路由、重复 operationId、同名组件冲突、手写 401）
各有单测覆盖，其中「手写 401 必须被拒绝」与「同名组件内容不同必须报错」是反向用例——
它们证明检查会失败，而不是只会通过。

## 补充：冻结 §11 时的三个实测发现（P8 2.2，2026-11-21）

冻结 6 个产品接口时，生成链暴露了三个在原 4 个端点下不会出现的问题。三条都改了机制，
不是改文档绕过去：

1. **嵌套类型走 `$defs`，不在原地展开**。`schemars` 对 `TargetRef` 这类被引用的类型输出
   `{"$ref":"#/$defs/TargetRef"}`，定义放在根上的 `$defs`——那不是合法的 OAS 引用位置，
   文档会出现「`$ref` 指向不存在的组件」。现在 `register_body` 把 `$defs` 提升成
   `components.schemas` 并改写引用前缀（`lift_defs`），子类型因此只出现一次。
   反向用例：`nested_types_are_lifted_out_of_defs` 与 `plain_schemas_pass_through_unchanged`。
2. **带注释的枚举变体会变成 `oneOf` + `const`**。客户端生成器会得到 union 而不是 enum。
   处理方式是把变体说明上移到类型注释（`CoverageVerdictCode` / `ObservationKind`），
   schema 保持朴素字符串枚举。**撤销条件 2 因此仍不成立**——契约里没有出现
   `oneOf`/`anyOf`/`discriminator`。
3. **一份文档、两张路由表**：`document_many` 接收多个路由表并合并成一份契约。
   控制面（`/_bod/*`，本机会话密钥）与产品接口（`/api/v1/*`，§34.12 主体判定）
   的鉴权语义不同，混进一张表会让文档把两者写成同一种。分发由
   `host_handshake::handle` 按 `delivery_api::PREFIX` 分流；测试断言
   「前缀下的路由都在产品表、控制面一条都不在前缀下、两表无重复 (方法,路径)」。

同一轮还加了 `Contract.status`（`served` / `planned`，无默认值）与文档里的
`x-bod-status`：§11 的 6 个操作当前是 `planned` + 501，实现落地必须改成 `served`
并删掉 501，测试双向断言。这是「路径已冻结」与「已经能用」分开说的机械保障。

## 撤销条件（可机械判断，任一满足即重评）

1. **需要交互式 API 文档或客户端代码生成流水线**（要 Swagger UI、要 `openapi-typescript`
   在 CI 里跑）：此时 utoipa 的生态收益成立，重评为 A。
2. **`openapi.rs` 超过 400 行**，或契约需要 `oneOf`/`anyOf`/`discriminator`/多态请求体：
   说明外层结构长出了 schemars 覆盖不到的表达，改为引入专门的 OAS 库。
3. **schemars 出现安全通告或 MSRV 高于本仓下限**：换实现，`schema_of` 的函数签名不变。

判断方式：1 看 `apps/workbench/package.json` 是否新增 OAS 相关依赖与脚本；
2 看 `wc -l crates/bod-app/src/openapi.rs` 与是否出现 `OneOf` 之类的类型；3 看依赖通告。

## 代码体现

- `crates/bod-app/src/openapi.rs`：`schema_of` / `document` / `operation_id` / `parameters_of`。
- `crates/bod-app/src/http_router.rs`：`Contract`、`ContractBody`、`ContractResponse`、
  `QueryParam`、`Implementation`、`pattern_parameters`。
- `crates/bod-app/src/host_handshake.rs`：`ROUTES` 每条带 `contract`；`openapi_document()`
  合并控制面与产品接口两张表；`handle` 按前缀分流。
- `crates/bod-app/src/delivery_api.rs`：§11 的 6 个操作（`planned`）+ 请求/响应 DTO。
- `crates/bod-app/src/host_api.rs`：4 个响应体 + `HandshakeRequest`，均 `JsonSchema`。
- `verification/openapi.json`：入库的生成物（1,459 行，10 个操作，26 个组件）。
- 重生成：`BOD_BLESS_OPENAPI=1 cargo test -p bod-app --test openapi_contract`
