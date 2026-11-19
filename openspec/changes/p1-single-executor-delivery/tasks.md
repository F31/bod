# Tasks: P1 单执行者交付

> 规格来源：本 Change 的 `specs/*/spec.md`。依赖 P0 Change 的可信核心；不实现第二执行端、负责人切换、影响图或远程执行。所有 I/G 绑定初始为 planned。

## 1. 工作区与 Git 基线

- [ ] 1.1 实现每 Attempt 独立 worktree/分支与唯一写目录，验证 `bod-app::workspace_safety` 的 G-29：两个并行 Attempt 不共用暂存区、不读取对方未提交内容。
- [ ] 1.2 实现 write_scope/coordinated_scope 声明与提交 diff 门禁，拒绝软链接穿越、Git 管理目录与未授权共享配置，验证越界候选进入 NeedsRevision 且列出具体路径。
- [ ] 1.3 实现共享资源协调清单（公共类型/API schema/migrations/根依赖/OpenSpec），验证同一 migration ID 冲突被阻断且按稳定 ID 分配。
- [ ] 1.4 实现工作区生命周期与保留策略，终态后回收但保留脏文件/未确认现场引用，验证取消后用户仓库未被删除且可查询待核实项。
- [ ] 1.5 实现测试端口、临时目录与构建输出隔离，验证两个工作区并发启动测试服务无端口冲突或跨 Company 数据读取。

## 2. Context 与 Profile

- [ ] 2.1 实现 ContextBundle 清单与确定装配，覆盖 spec/profile/skill/preference/input/adapter/runtime/assembly 版本与 omitted_refs，验证 `bod-app::context_handoff` 的 `invariant_i_17`：相同输入生成相同清单与摘要。
- [ ] 2.2 实现按 revision/digest 的不可变片段缓存与权限复核，验证撤权后缓存命中被拒绝并重新装配（G-48 的同类断言）。
- [ ] 2.3 实现指令优先级与冲突检测，外部网页/Issue/工具输出标不可信，验证工具输出要求改验收时形成 Question 且原验收不变。
- [ ] 2.4 实现会话复用条件与角色变更重置，验证同一 Agent 跨两个项目的会话、工作区与密钥分离（`invariant_i_36`）。
- [ ] 2.5 记录上下文装配与缓存失效说明到 `docs/spec-notes/p1-context.md`，验证其中每个缓存对象与失效条件可追溯到本 Change 的 requirement。

## 3. 讨论与澄清

- [ ] 3.1 实现 Issue 讨论记录（author/时间/关联修订/来源与编辑历史），验证评论编辑保留原文与修订。
- [ ] 3.2 实现评论无副作用与 Question→NeedsInput，验证 `invariant_i_38`：LGTM 不结单、不派发、不授权，Question 记录答复主体与重新唤醒条件。
- [ ] 3.3 实现有权限的评论→Decision/FeedbackProposal 显式转换与外部回调认证，验证伪造批准回调不产生接受/发布效果，转换后决策绑定对象与修订。
- [ ] 3.4 编写澄清状态与超时处理说明到 `docs/spec-notes/p1-clarity.md`，验证每个稳定状态与恢复条件对应具体 scenario。

## 4. 直派闭环与工作台

- [ ] 4.1 实现首次使用路径与自动最小工作约定，验证无 Team/WorkPackage/Run 配置即可完成一次真实仓库指派（`invariant_i_10`）。
- [ ] 4.2 打通真实仓库实现→本地验证→人工/预授权接受闭环，验证 `bod-app::coding_integration` 的 `invariant_i_18`、`invariant_i_20`：接受绑定确定提交/规格/报告，桌面重启后仍可查询。
- [ ] 4.3 实现取消请求/确认停止分离与 Unknown 对账入口，验证 cancel_ack 不释放占用、不新建 Attempt，界面显示待核实与处理人（`invariant_i_11`）。
- [ ] 4.4 实现本地验证报告绑定与备份核验，验证自报通过但无报告时验收保持 Pending/NeedsRevalidation，备份缺 WAL/CAS 引用不标可恢复（G-6、G-9）。
- [ ] 4.5 实现工作台首页四问与任务页证据视图（只投影宿主事实、无第二 DB），验证待决定事项含阻塞主体、所需决定与可采取动作，任务页显示拒绝原因与证据（`invariant_i_55`）。
- [ ] 4.6 实现预算耗尽时阻止新派发并保留已接受成果，验证 InsufficientBudget 稳定状态与批准入口可见且不创建 Binding（P0 G-45 回归）。
- [ ] 4.7 编写 P1 首次使用与交付操作手册到 `docs/spec-notes/p1-user-flow.md`，验证手册每一步都能在工作台复现且标注了 Unknown/取消的诚实表达。

## 5. P1 验证与出口

- [ ] 5.1 运行 `bod-app::context_handoff`、`bod-app::workspace_safety`、`bod-app::coding_integration`、`bod-app::resource_state` 的 P1 用例，验证 G-11、G-15、G-16、G-29 均有命名 case 与真实断言。
- [ ] 5.2 在同一真实仓库与一种已接入 Agent 上完成一次完整交付演示，验证任务、验证、接受、备份四段证据绑定同一修订且报告注明 commit/环境。
- [ ] 5.3 汇总 P1 出口证据与支持矩阵，验证「无 Team/Run 配置完成真实任务」「取消与 Unknown 如实显示」两项有可核对报告，未测项保持 Target/Unverified。

## Workflow follow-up

- 评审通过后归档本 Change，并把 4 份 delta 同步到 `openspec/specs/`；归档前不把 P1 能力标为 Supported。
- 后续 P2—P7 以新 Change 承接，不修改本 Change 的历史约定。
