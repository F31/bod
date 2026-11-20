# Tasks: P3 变更与有效协作

> 规格来源：本 Change 的 `specs/*/spec.md`。依赖 P1 工作区与 P2 接续；不实现 CI/环境发布、团队 Catalog、远程或移动。所有 I/G 绑定初始为 planned。

## 1. OpenSpec 集成与任务映射

- [x] 1.1 实现 Change 目录到项目 Change 的映射与 SpecRevision/RequirementRef/ScenarioRef 记录，验证 `invariant_i_10`：工作包引用确定 spec_commit，文件后续改动不静默改变输入。
- [x] 1.2 实现 task-map 稳定 ID、锚点、指纹与匹配状态，验证任务重排保持 ID、语义变化进入 Unresolved 且不按行号绑定（`invariant_i_13`）。
- [x] 1.3 实现工作包拆分提案与覆盖/循环/共享冲突/大小检查，验证机械按 Agent 数量均分被拒绝。
- [x] 1.4 实现 tasks.md 勾选为进度投影与手工勾选导入为观察，验证 `invariant_i_13`：勾选不把工作包标为 Accepted。
- [x] 1.5 实现单一归档协调入口与同 requirement 多 Change 串行协调，验证两 Change 文本无冲突时仍串行同步规格。
- [x] 1.6 编写 P3 规格映射与归档说明到 `docs/spec-notes/p3-openspec.md`，验证每个映射字段可追溯到本 Change requirement。

## 2. 影响评估与选择性重验

- [x] 2.1 实现 Revision 依赖边类型与来源/置信记录，验证 `invariant_i_53`：推测边不自动升级为确定事实。
- [x] 2.2 实现 ImpactAssessment 四类区域与评估快照（基线/关系/规则/触发/时间），验证缺少依赖的区域标 unknown 而不是不受影响。
- [x] 2.3 实现选择性重验提案与授权策略/人工接受门槛，验证接口变化只生成相关组件重验且无关报告不进入候选。
- [x] 2.4 实现重验幂等键、预算、范围与循环限制，验证 `invariant_i_54`：短时重复变化只生成一个提案且不无限派发。
- [x] 2.5 实现维护提案与 unknown/stale 观测，验证断网或依赖变化时生成待核实提案、不自动升级或发布。
- [ ] 2.6 以真实接口变化样本运行 G-31，验证报告列出确定/可能/未知区域、重验依据与未执行项。

## 3. 多工作区与受控合并

- [x] 3.1 实现每 Attempt 独立 worktree/分支与主干受控更新，验证两候选互不读取未提交内容且主干 ref 只由合并入口更新（`invariant_i_20`）。
- [x] 3.2 实现共享资源责任者与契约冲突门禁，验证两个工作包修改同一公共 schema 时合并被阻止。
- [x] 3.3 实现合并队列串行化与最新基线重建，验证主干在候选验证期间更新后重新构造候选并重新验证。
- [x] 3.4 实现合并证据与实际组合绑定，验证 `invariant_i_10`：证据中的 target commit 与更新 ref 不一致时拒绝接受。
- [x] 3.5 实现写路径/软链接/Git 管理目录限制，验证 worktree 只能声明为协作隔离，不能通过适配声明标为 sandboxed（G-29）。
- [x] 3.6 编写 P3 合并流程与冲突处理说明到 `docs/spec-notes/p3-merge.md`，验证流程中每个门禁对应 scenario 与测试用例。

## 4. 协作计划与扁平规则

- [x] 4.1 实现 PlanProposal/PlanRevision 分离与顾问/负责人权限边界，验证顾问派发命令被拒、负责人只能在 Grant 内安排。
- [x] 4.2 实现执行图准入校验（循环、需求/场景映射、共享资源、权限），验证循环依赖图被拒且不创建 Ready 项。
- [x] 4.3 实现在途 Run 保护与计划修订迁移/新 Run，验证运行中修改计划不影响在途 Run。
- [x] 4.4 实现共享 Agent 会话隔离与资源键扩展，验证同一 Agent 跨项目使用不同会话/工作区/ContextBundle 且并发上限分别计算。
- [x] 4.5 实现协作建议的依据记录与未知关系展示，验证无依据的并行建议不被自动接受为执行图。

## 5. P3 验证与出口

- [x] 5.1 运行 `bod-app::coding_integration`、`bod-app::impact_revalidation`、`bod-app::workspace_safety` 的 P3 用例，验证 G-7、G-31、G-29 有命名 case 与真实断言。
- [ ] 5.2 在真实项目上演示一次共享接口变化从发现到再次接受的完整过程，验证记录漏项、无效重验、协调与集成失败数据（`invariant_i_53`、`invariant_i_54`）。
- [x] 5.3 汇总 P3 出口证据与支持矩阵，验证「接口变化跨前后端/测试跟进」「共享 Agent 不串会话」「无强制管理层」三项有可核对报告，未测组合保持 Planned。

## Workflow follow-up

- 评审通过后归档本 Change，把 4 份 delta 同步到 `openspec/specs/`；归档前不把影响分析或多 Agent 合并标为 Supported。
- P4—P7 以新 Change 承接，不修改本 Change 的历史约定。
