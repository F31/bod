import { closeOutcome, describe, parseCloseAction, redact } from "./connection.js";
import type { ConnectionState } from "./connection.js";

/** Tauri 2 在窗口注入的内部调用通道；不在 Tauri 里时为 undefined。 */
interface TauriInternals {
  invoke<T>(command: string, args?: Record<string, unknown>): Promise<T>;
}

interface RawConnectionStatus {
  address: string;
  pid: number;
  instance_id: string;
  spawned_by_desktop: boolean;
  close_action: string;
}

function tauri(): TauriInternals | undefined {
  return (window as unknown as { __TAURI_INTERNALS__?: TauriInternals })
    .__TAURI_INTERNALS__;
}

function render(id: string, text: string): void {
  const node = document.getElementById(id);
  if (node) {
    node.textContent = text;
  }
}

function toState(raw: RawConnectionStatus): ConnectionState {
  return {
    address: raw.address,
    pid: raw.pid,
    instanceId: raw.instance_id,
    spawnedByDesktop: raw.spawned_by_desktop,
    closeAction: parseCloseAction(raw.close_action),
  };
}

async function main(): Promise<void> {
  const bridge = tauri();
  if (!bridge) {
    // 用浏览器直接打开时不要假装连上了宿主。
    render("state", "未在桌面壳中运行：没有可用的本地控制面通道");
    render("action", "headless 预览不建立连接，也不读取任何凭据");
    return;
  }

  try {
    const raw = await bridge.invoke<RawConnectionStatus>("connection_status");
    const state = toState(raw);
    render("state", describe(state));
    render("action", closeOutcome(state).reason);
  } catch (error) {
    // 错误文本可能混入凭据，展示前统一脱敏。
    render("state", "连接失败");
    render("action", redact(error instanceof Error ? error.message : String(error)));
  }
}

void main();
