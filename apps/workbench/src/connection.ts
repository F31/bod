/**
 * 桌面连接状态的纯逻辑：与 `bod-desktop::connection` 的语义一一对应。
 *
 * 三条规则在这里也必须成立，否则界面会与后端行为不一致：
 * 1. 未知 `close_action` 一律按 `detach`（最保守，不停宿主）。
 * 2. 外部已有宿主永不允许停止，即使配置写了 `stop_host`。
 * 3. 会话密钥不进入界面、不进入日志。
 */

export type CloseAction = "detach" | "stop_host";

/** 后端 `connection_status` 命令返回的形状（不含任何密钥）。 */
export interface ConnectionState {
  address: string;
  pid: number;
  instanceId: string;
  spawnedByDesktop: boolean;
  closeAction: CloseAction;
}

export interface CloseOutcome {
  /** 是否真的会向宿主发送停止请求。 */
  stop: boolean;
  /** 可展示给用户的判定原因。 */
  reason: string;
}

/** 未知值回落到 detach，绝不回落到 stop_host。 */
export function parseCloseAction(raw: unknown): CloseAction {
  return raw === "stop_host" ? "stop_host" : "detach";
}

/** 关闭窗口时的行为判定，与 Rust 侧 `close_connection` 的分支顺序一致。 */
export function closeOutcome(state: ConnectionState): CloseOutcome {
  if (state.closeAction === "detach") {
    return {
      stop: false,
      reason: `close_action=detach：断开 ${state.address}，宿主继续运行`,
    };
  }
  if (!state.spawnedByDesktop) {
    return {
      stop: false,
      reason: `宿主 pid=${state.pid} 不是本次桌面启动，即使配置 stop_host 也保持运行`,
    };
  }
  return {
    stop: true,
    reason: `stop_host：请求停止本次启动的宿主 pid=${state.pid}`,
  };
}

/**
 * 失败时可能带出后端错误文本；统一脱敏，避免把凭据渲染到界面或控制台。
 * 覆盖 `sk-...` 形式与 64 位十六进制（本项目的令牌/会话密钥形状）。
 */
export function redact(text: string): string {
  return text
    .replace(/\bsk-[A-Za-z0-9_-]{4,}/g, "<redacted>")
    .replace(/\b[0-9a-f]{64}\b/g, "<redacted>");
}

/** 供界面展示的一行事实；不含密钥字段，因为入参里就没有。 */
export function describe(state: ConnectionState): string {
  const owner = state.spawnedByDesktop ? "本次桌面启动" : "已有宿主（外部）";
  return `${state.address} · pid ${state.pid} · ${state.instanceId} · ${owner}`;
}
