# Design: P1 单执行者交付

## Context

动机见 `proposal.md`；P0 已提供可信宿主、准入、队列、Effect Gate 与最小接受。P1 只增加一个真实运行时和一个真实仓库的交付路径，不增加第二个执行端或管理层（§40.1、§40.7 D5）。

## Goals / Non-Goals

- 目标：一个用户用已有 Agent 对真实仓库完成一次被接受的交付；同一规格修订在不同 Attempt 可复现装配。
- 目标：取消、Unknown、脏工作区和未确认费用如实显示，并有备份/恢复证据。
- 非目标：多 Agent 合并队列、第二运行时、负责人切换、CI 制品与生产发布、团队权限、远程节点。
- 非目标：把执行端的内部推理或私有记忆纳入 bod 治理范围。

## Decisions

1. **每 Attempt 一个 worktree/分支。** 工作区绑定 attempt_id 与租约；仓库级锁只覆盖 ref/worktree 元数据更新，不覆盖整个开发时段（§27.1）。替代方案是共享可写目录，会污染证据并破坏隔离。
2. **写范围声明 + diff 校验。** write_scope 与 coordinated_scope 在派发前冻结；路径门禁检查软链接穿越、Git 管理目录与共享配置。强隔离由 runner 实测能力决定，worktree 只算协作隔离（§20.10、§27.3）。
3. **ContextBundle 只引用不可变修订。** 清单含 spec/profile/skill/preference/input/adapter/runtime/assembly 版本与 omitted_refs；命中缓存也必须通过当前 Company/Project 与权限复核（§18.1、§22.4）。替代方案是缓存渲染后的全文，会绕过撤权和版本失效。
4. **指令优先级确定。** 用户目标与验收 > 项目授权规则 > Profile > 偏好 > 外部材料；冲突形成 Question，不静默拼接（§18.3）。
5. **评论与决策分离。** 普通 Issue 评论是事实记录，不改变状态、权限或验收；转化为 Decision/FeedbackProposal 需要显式有权限命令并绑定修订（§30.7）。
6. **直派默认路径。** Issue 直接指派即自动生成一个最小工作包与 Run，用户不配置 Team、WorkPackage 或 Run；复杂拆分留给 P3（§5.7、§7.2）。
7. **工作台只投影宿主事实。** 首页四个问题与任务页从宿主状态、证据和待决定计算；桌面不另建 DB，不把进程退出显示为验收完成（§1.3、§36.1）。
8. **取消与 Unknown 分列。** cancel_ack 与 stop_confirmed 分开；Unknown 保留占用与现场，显示待核实与下一步（§10.6、§15.6）。

## Risks / Trade-offs

- [worktree 仍有共享 Git 元数据] → 只授予只读对象缓存权限，凭据隔离；需要强隔离时声明为已知缺口并限制任务（§27.1）。
- [上下文装配成本] → 固定片段按 revision 复用，缓存 key 含身份与版本；不牺牲隔离换命中率（§22.4）。
- [澄清循环过长] → NeedsInput 设稳定超时与指定答复主体，超时升级为待决定而非无限等待。
- [真实仓库样本副作用] → 使用脱敏 fixture 和本地临时仓库；任何外部写入走 Effect Gate，不接受 Agent 自述通过。

## Migration Plan

在 P0 的 SQLite 上追加增量迁移：workspaces、context_bundles、questions/answers、direct_delivery_evidence、backup_records。旧 Issue 无工作区/上下文引用时显示未知，不伪造历史现场。回滚保留 P0 数据与备份，恢复后 Unknown 先对账。

## Open Questions

- 首个真实仓库样本的仓库地址与脱敏范围由试点授权确定；不改变工作区与证据契约。
- 工作台的视觉布局可在试点中调整，但四个首页问题与证据绑定关系属于规格，不在本层变动。
