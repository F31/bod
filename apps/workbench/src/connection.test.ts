import test from "node:test";
import assert from "node:assert/strict";

import { closeOutcome, describe, parseCloseAction, redact } from "./connection.js";
import type { ConnectionState } from "./connection.js";

const base: ConnectionState = {
  address: "127.0.0.1:53001",
  pid: 4100,
  instanceId: "inst-abc",
  spawnedByDesktop: false,
  closeAction: "detach",
};

test("parseCloseAction 保留受支持的枚举值", () => {
  assert.equal(parseCloseAction("stop_host"), "stop_host");
  assert.equal(parseCloseAction("detach"), "detach");
});

test("parseCloseAction 对未知值回落 detach，而不是停宿主", () => {
  assert.equal(parseCloseAction("StopHost"), "detach");
  assert.equal(parseCloseAction(undefined), "detach");
  assert.equal(parseCloseAction(42), "detach");
});

test("detach 永远不停宿主", () => {
  const outcome = closeOutcome({ ...base, spawnedByDesktop: true });
  assert.equal(outcome.stop, false);
  assert.match(outcome.reason, /detach/);
});

test("外部已有宿主即使配置 stop_host 也不停止", () => {
  const outcome = closeOutcome({
    ...base,
    closeAction: "stop_host",
    spawnedByDesktop: false,
  });
  assert.equal(outcome.stop, false);
  assert.match(outcome.reason, /不是本次桌面启动/);
});

test("只有自己启动且配置 stop_host 才停止", () => {
  const outcome = closeOutcome({
    ...base,
    closeAction: "stop_host",
    spawnedByDesktop: true,
  });
  assert.equal(outcome.stop, true);
  assert.match(outcome.reason, /pid=4100/);
});

test("redact 抹掉 sk- 前缀与会话密钥形状的十六进制串", () => {
  const secret = "a".repeat(64);
  const cleaned = redact(`handshake failed token=sk-live-9f2c key=${secret}`);
  assert.equal(cleaned.includes("sk-live-9f2c"), false);
  assert.equal(cleaned.includes(secret), false);
  assert.match(cleaned, /<redacted>/);
});

test("redact 不误伤普通地址与 pid", () => {
  assert.equal(redact("127.0.0.1:53001 pid=4100"), "127.0.0.1:53001 pid=4100");
});

test("describe 区分外部宿主与本次启动", () => {
  assert.match(describe(base), /已有宿主（外部）/);
  assert.match(describe({ ...base, spawnedByDesktop: true }), /本次桌面启动/);
});
