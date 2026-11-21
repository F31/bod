# ADR-0005 分层口径：按依赖方向划分，不按「纯不纯」划分

- 状态：`accepted`
- 日期：2026-11-21
- 决策者：项目负责人（评审后授权按优先级推进）
- 关联：V2.9.8 §3 代码分层、ADR-0003（依赖方向用机械守卫）、评审报告 CR-008

## 场景

V2.9.8 §3 写「`bod-core` = 值类型、领域命令、纯判定与状态演进」。
评审时发现实际口径与这句话不完全对应，需要写清楚，否则每次评审都要重新争一遍。

实测（2026-11-21，命令见文末）：

| 项 | 读数 |
|---|---|
| `bod-core` | 7 个文件 / 5,593 行；`[dependencies]` 为空 |
| `bod-app` | 86 个模块（含 `lib.rs` 外的全部 `src/*.rs`）/ 95,750 行 |
| 其中「源码里没有直接 IO 调用」的模块 | 51 |

也就是说：**一半以上的纯判定模块留在 `bod-app`**，`bod-core` 只装了其中 6 个。

## 决策

分层按**依赖方向**划分，不按「是否纯」划分：

1. `bod-core` 的判据是**零外部依赖**（`[dependencies]` 为空），不是「代码是否只做计算」。
2. 一个纯判定模块要迁进 `bod-core`，必须同时满足：
   - 不引用任何外部 crate（包括 `serde`）；
   - 至少被两个层使用，或它的隔离对测试有实质价值；
   - 有人真的需要这层隔离（而不是「看起来更整齐」）。
3. 不满足就留在 `bod-app`。**「纯」本身不是迁移理由。**

## 理由

1. **`serde` 是分界线，不是纯度**。绝大多数领域类型要实现 `Serialize`/`Deserialize`
   才能进契约、进库、进日志。`bod-core` 零依赖 ⇒ 迁进去就失去序列化，
   于是要么把类型复制一份，要么给 core 加 `serde` 依赖——后者会让 ADR-0003 的守卫失去意义。

2. **迁移的成本是真实的，收益是审美上的**。51 个模块迁走意味着重写全部 `use` 路径、
   调整可见性、拆循环依赖。换来的是「目录更整齐」，而测试、编译期检查、契约一致性
   一个都不会因此变好。

3. **真正需要防的是反向依赖，不是位置**。`bod-app` 依赖 `bod-core` 是合法方向；
   反过来不合法。这条已被 `xtask verify --stage P0` 机械守卫（ADR-0003），
   与「纯判定放哪」无关。

## 代价

- `bod-app` 会持续变大（已 95,750 行 / 86 模块），单文件职责靠 CR-005（`store.rs` 拆分）
  与 CR-008（按对象组分子目录）解决，不靠迁 core。
- 「这个模块该放哪」仍然需要判断，但判据从「纯不纯」变成「有没有外部依赖」——
  后者可以直接看 `use`，不需要辩论。

## 撤销条件（可机械判断，任一满足即重评）

1. `bod-core` 的 `[dependencies]` 不再为空（说明「零依赖」这条已放弃，分层口径要重写）；
2. `bod-app` 单 crate 编译时间超过工作区总编译时间的 70%（说明规模已到必须拆分的程度）；
3. 出现「多个 crate 需要同一份纯判定」的真实需求（当前 `apps/desktop/shell` 不直接用它）。

判断方式：1 看 `crates/bod-core/Cargo.toml`；2 用 `cargo build --timings` 看占比；3 看新 crate 的 `use`。

## 代码体现

- `crates/bod-core/Cargo.toml`：`[dependencies]` 为空。
- `xtask/src/main.rs`：`verify --stage P0` 的 `forbidden_rules()`。
- 本条不要求任何代码迁移。

## 复现命令

```bash
# core 依赖为空
sed -n '/\[dependencies\]/,$p' crates/bod-core/Cargo.toml

# 模块数与「无直接 IO」数（grep 启发式，含间接经 store 等的 IO 未计入）
ls crates/bod-app/src/*.rs | grep -v lib.rs | wc -l
for f in crates/bod-app/src/*.rs; do case "$f" in */lib.rs) continue;; esac;
  grep -qE "std::fs|std::net|std::process|rusqlite|std::io::|TcpStream|Command::new" "$f" || echo "$f"; done | wc -l
```

启发式的局限要写明：它只排除**直接**出现 IO 调用的模块，不排除「通过 `store::` 等
间接访问 IO」的模块。所以 51 是上界，不是精确值。
