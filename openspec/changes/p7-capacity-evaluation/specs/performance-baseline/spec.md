# Spec Delta: performance-baseline

## Purpose

定义跨平台、跨形态的可复现实测与预算冻结流程，使性能结论有明确条件、样本和限制，而不是由设计目标或单一指标代替。

## ADDED Requirements

### Requirement: 测量条件记录

每次性能实测 SHALL 记录 OS、CPU、Rust 与构建参数、SQLite 同步策略、数据规模、缓存冷热、平台与运行版本。首轮统一 Linux x86_64、8 核、16 GiB RAM、本地 NVMe；Windows/WSL2 SHALL 单独报告。

#### Scenario: 复现实测
- **WHEN** 另一名工程师用同一命令与样本重跑一次基线
- **THEN** 报告可对照 OS/CPU/构建参数/同步策略/数据规模，差异项被明确列出

### Requirement: 分形态报告

headless、可选 Web 宿主、Tauri 桌面与移动发布 SHALL 分别报告压缩/展开体积、平台、静态/动态链接、TLS 与 SQLite 构建方式及资源占用；Web 宿主开销 MUST NOT 冒充 Tauri 桌面总内存，headless 空闲值 MUST NOT 直接套到桌面。

#### Scenario: 桌面资源结论
- **WHEN** 有人用 headless 空闲 RSS 宣称桌面总内存达标
- **THEN** 该结论被视为无效，要求分别测壳、WebView、宿主 sidecar、管理进程树与外部 Agent

### Requirement: 测量项与口径

基线 SHALL 覆盖空闲 RSS、冷启动到 API ready、关键事务 p99、事件写入吞吐、就绪候选选择 p99、状态到 Web 可见 p95、十万事件后重启 ready，以及发布包体积。每项 MUST 给出样本数、环境、命令与失败原因；吞吐目标与尾延迟目标 MUST NOT 声称同时达成。

#### Scenario: 吞吐量与尾延迟
- **WHEN** 一次事件吞吐测试达到探索目标但关键事务 p99 超标
- **THEN** 报告分别记录两项结果，不宣称全部预算达标

### Requirement: 目标与验证状态

§38.2 的数值 SHALL 在实测前标为 Target/Unverified；只有对应平台、配置与样本的报告存在时才可标 Measured。连续两个可比较阶段实测后 SHALL 才能冻结正式预算，冻结记录 MUST 含版本与适用范围。

#### Scenario: 冻结前宣称达标
- **WHEN** 团队仅凭设计目标把空闲 RSS 标为已达标
- **THEN** 系统要求实测报告与样本，未测项保持 Target/Unverified

### Requirement: 不降低正确性

超标时 SHALL 先定位默认依赖、缓存策略与结构；MUST NOT 为达标静默关闭 WAL 同步、放宽事务边界、跳过授权或降低接受门禁。任何相关变更 MUST 回归原 I/G 门禁。

#### Scenario: 通过降同步达标
- **WHEN** 一次优化建议把 SQLite 同步改为更低持久性以改善尾延迟
- **THEN** 该建议被阻断或标为需显式风险授权与完整回归，不直接计入预算达标

### Requirement: 报告归档

每次实测 SHALL 保存命令、配置、环境、样本生成方式、原始输出、摘要与 commit；报告 MUST 能由第三方在相同条件下复核，缺失数据标 Unverified 而不是零或空白。

#### Scenario: 缺样本报告
- **WHEN** 一份性能报告没有样本数与命令
- **THEN** 它不进入容量决策输入，相关指标保持 Unverified
