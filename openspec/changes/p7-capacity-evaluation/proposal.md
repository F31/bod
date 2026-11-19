# Proposal: P7 容量评估

## Why

P0—P6 建立了完整能力，但没有真实负载证据就容易凭日事件数或单个恢复指标决定引入 PostgreSQL、分布式执行或外部持久执行服务（§40.4）。文档明确要求按实测 RTO、写争用、数据体积与多机需求评估，并把方案对比与瓶颈记录下来；没有这一门，优化可能静默降低持久性、授权或验收语义。

## What Changes

- 建立性能预算实测基线：统一首轮平台与记录项（OS/CPU/Rust/构建参数/SQLite 同步策略/数据规模/缓存冷热），分别测量 headless、可选 Web、Tauri 桌面与移动形态，不把 headless 数字当桌面承诺。
- 建立容量毕业门：用真实 RTO、单写争用、索引/数据体积与多机需求判断是否需要变更存储或执行引擎；不按「一百万事件/日」机械迁移。
- 记录候选方案对比（索引/批处理/存储优化、执行节点扩展、PostgreSQL、外部持久执行服务）及迁移代价、运维负担与故障语义，形成可审计决定。
- 连续两个可比较阶段实测后冻结正式预算；超标先定位默认依赖和结构，不通过降低持久性或验收语义凑数字。
- 非目标：预先引入 PostgreSQL 或分布式引擎、修正在测 bug、扩大功能范围、把目标值写成已达成承诺。

## Capabilities

### New Capabilities

- `performance-baseline`: 跨平台、跨形态的可复现实测与预算冻结流程。
- `capacity-governance`: 基于真实瓶颈的方案对比、毕业门与可审计引擎决策。

### Modified Capabilities

无。P0—P6 Change 尚未归档；本 Change 以新 capability 路径建立 P7 评估基线，不改写既有历史约定。

## Impact

- 代码/工具：`xtask bench`/诊断命令、测试样本生成与报告归档；不改业务语义，除非决策门批准后续 Change。
- 数据：新增 performance_runs、capacity_assessments、graduation_decisions、支持矩阵更新记录。
- 验证：引用 §38.2 性能预算与 §40.4 规模毕业条件；本 Change 的通过条件是实测报告与决定记录，不是功能实现声明。
- 依赖：P0—P6 的 I/G 报告、实际支持矩阵与故障样本必须存在；未声明支持的能力不进入容量结论。
