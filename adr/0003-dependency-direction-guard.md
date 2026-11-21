# ADR-0003 依赖方向用机械守卫，不靠自觉

- 状态：`accepted`
- 日期：2026-11-19（P0 归档时确立，2026-11-21 从工程档案提为独立文件）
- 决策者：项目负责人
- 关联：AGENTS.md 禁止行为第 7 条、`xtask verify --stage P0`

## 场景

模块化单体最大的风险不是「选错框架」，而是**分层缓慢腐化**：某次改动为了方便，
让 `bod-core` 里出现一个 `rusqlite` 的 import，或让 `bod-app` 反向依赖 CLI。
这类越界不会让测试变红，只会在半年后让内核无法脱离 IO 测试。

82 个平铺模块的 import 关系，靠 code review 记不住。

## 决策

1. `bod-core` 保持**零外部依赖**（`[dependencies]` 为空）。
2. 依赖方向由 `cargo run -p xtask -- verify --stage P0` 机械校验，违例必须阻断合并。
3. 守卫本身要能证明自己有效：`--self-test` 注入一个违例，必须被捕获。

## 理由

- 「守卫能失败」才算守卫。只报告不拦截的检查会退化成日志，没人看。
- 规则表可枚举：分层规则本身不多（core 不许依赖 DB/Web/Tauri/Agent SDK/上层 crate），
  维护成本远低于人工 review 全量 import。

## 代价

- 每次新增依赖要通过规则表登记（AGENTS.md 第 8 条），对「只想快点写完」的场景是摩擦。
- `forbidden_rules()` 需要人工维护：新增 crate 时要同步更新，否则默认放行。

## 撤销条件（可机械判断）

1. `forbidden_rules()` 的条目数与 `crates/*/Cargo.toml` 里的 crate 数长期不一致
   （说明规则表已经跟不上仓库结构，需要重新设计而不是继续打补丁）；
2. `--self-test` 无法再构造出违例（说明守卫已与实际构建解耦，形同虚设）。

判断方式：跑 `cargo run -p xtask -- verify --stage P0 --self-test` 看是否仍能捕获注入违例。

## 代码体现

- `xtask/src/main.rs`：`verify --stage P0`、`forbidden_rules()`、`--self-test`。
- `crates/bod-core/Cargo.toml`：`[dependencies]` 为空。
