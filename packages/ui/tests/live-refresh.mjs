// Node 24 直接剥离 TypeScript；虚拟时钟与 EventTarget 验证真实调度器，无 DOM/计时依赖。
import assert from "node:assert/strict";
import { afterEach, beforeEach, test } from "node:test";
import { startLiveRefresh } from "../src/useLiveRefresh.ts";

class BrowserTarget extends EventTarget {
  hidden = false;
  listeners = new Map();

  addEventListener(type, callback, options) {
    super.addEventListener(type, callback, options);
    if (!this.listeners.has(type)) this.listeners.set(type, new Set());
    this.listeners.get(type).add(callback);
  }

  removeEventListener(type, callback, options) {
    super.removeEventListener(type, callback, options);
    this.listeners.get(type)?.delete(callback);
  }

  get listenerCount() {
    return [...this.listeners.values()].reduce((sum, listeners) => sum + listeners.size, 0);
  }
}

function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

// 完成 await load 与其 finally；不推进虚拟时间。
async function settle() {
  for (let i = 0; i < 5; i += 1) await Promise.resolve();
}

let page;
let browser;
let stops;
let savedDocument;
let savedWindow;

function start(load, interval = 2_000) {
  const stop = startLiveRefresh(load, interval);
  stops.push(stop);
  return stop;
}

function visibility(hidden) {
  page.hidden = hidden;
  page.dispatchEvent(new Event("visibilitychange"));
}

function focus() {
  browser.dispatchEvent(new Event("focus"));
}

beforeEach((t) => {
  savedDocument = Object.getOwnPropertyDescriptor(globalThis, "document");
  savedWindow = Object.getOwnPropertyDescriptor(globalThis, "window");
  page = new BrowserTarget();
  browser = new BrowserTarget();
  stops = [];
  Object.defineProperty(globalThis, "document", { configurable: true, value: page });
  Object.defineProperty(globalThis, "window", { configurable: true, value: browser });
  t.mock.timers.enable({ apis: ["setTimeout"] });
});

afterEach(() => {
  for (const stop of stops) stop();
  assert.equal(page.listenerCount, 0, "document listeners leaked");
  assert.equal(browser.listenerCount, 0, "window listeners leaked");
  if (savedDocument) Object.defineProperty(globalThis, "document", savedDocument);
  else delete globalThis.document;
  if (savedWindow) Object.defineProperty(globalThis, "window", savedWindow);
  else delete globalThis.window;
});

test("首次立即前台加载，后续每次后台加载共用可取消 signal", async (t) => {
  const calls = [];
  start(async (signal, background) => { calls.push({ signal, background }); });
  assert.equal(calls.length, 1);
  assert.equal(calls[0].background, false);
  assert.equal(calls[0].signal.aborted, false);
  await settle();
  t.mock.timers.tick(1_999);
  assert.equal(calls.length, 1);
  t.mock.timers.tick(1);
  assert.equal(calls.length, 2);
  assert.equal(calls[1].background, true);
  assert.equal(calls[1].signal, calls[0].signal);
  await settle();
  t.mock.timers.tick(2_000);
  assert.equal(calls.length, 3);
});

test("长请求完成后才计时，不按请求开始时间轮询或补发积压", async (t) => {
  const first = deferred();
  let calls = 0;
  start(() => { calls += 1; return calls === 1 ? first.promise : Promise.resolve(); });
  t.mock.timers.tick(20_000);
  focus();
  visibility(false);
  assert.equal(calls, 1);
  first.resolve();
  await settle();
  t.mock.timers.tick(1_999);
  assert.equal(calls, 1);
  t.mock.timers.tick(1);
  assert.equal(calls, 2);
});

test("后台请求同样不重叠，focus 与可见事件不排队重复请求", async (t) => {
  const pending = deferred();
  let calls = 0;
  start(() => { calls += 1; return calls === 2 ? pending.promise : Promise.resolve(); });
  await settle();
  t.mock.timers.tick(2_000);
  t.mock.timers.tick(10_000);
  for (let i = 0; i < 3; i += 1) { focus(); visibility(false); }
  assert.equal(calls, 2);
  pending.resolve();
  await settle();
  assert.equal(calls, 2);
  t.mock.timers.tick(2_000);
  assert.equal(calls, 3);
});

test("初始隐藏仍加载前台数据，但隐藏时不启动轮询", async (t) => {
  page.hidden = true;
  const calls = [];
  start(async (_signal, background) => { calls.push(background); });
  assert.deepEqual(calls, [false]);
  await settle();
  t.mock.timers.tick(20_000);
  focus();
  assert.deepEqual(calls, [false]);
  visibility(false);
  assert.deepEqual(calls, [false, true]);
});

test("隐藏清除待执行计时器，恢复可见立即刷新并重置间隔", async (t) => {
  let calls = 0;
  start(async () => { calls += 1; });
  await settle();
  t.mock.timers.tick(1_000);
  visibility(true);
  t.mock.timers.tick(20_000);
  assert.equal(calls, 1);
  visibility(false);
  assert.equal(calls, 2);
  await settle();
  t.mock.timers.tick(1_999);
  assert.equal(calls, 2);
  t.mock.timers.tick(1);
  assert.equal(calls, 3);
});

test("页面未发 visibilitychange 但已经隐藏时计时器也不请求", async (t) => {
  let calls = 0;
  start(async () => { calls += 1; });
  await settle();
  page.hidden = true;
  t.mock.timers.tick(2_000);
  assert.equal(calls, 1);
  visibility(false);
  assert.equal(calls, 2);
});

test("请求期间隐藏不取消当前加载，完成后保持暂停", async (t) => {
  const pending = deferred();
  let calls = 0;
  let signal;
  start((currentSignal) => { signal = currentSignal; calls += 1; return pending.promise; });
  visibility(true);
  assert.equal(signal.aborted, false);
  pending.resolve();
  await settle();
  t.mock.timers.tick(20_000);
  assert.equal(calls, 1);
});

test("可见窗口 focus 立即后台刷新，原计时器不会触发重复请求", async (t) => {
  const calls = [];
  start(async (_signal, background) => { calls.push(background); });
  await settle();
  t.mock.timers.tick(1_500);
  focus();
  assert.deepEqual(calls, [false, true]);
  await settle();
  t.mock.timers.tick(500);
  assert.equal(calls.length, 2);
  t.mock.timers.tick(1_500);
  assert.deepEqual(calls, [false, true, true]);
});

test("接受自定义刷新间隔", async (t) => {
  let calls = 0;
  start(async () => { calls += 1; }, 750);
  await settle();
  t.mock.timers.tick(749);
  assert.equal(calls, 1);
  t.mock.timers.tick(1);
  assert.equal(calls, 2);
});

test("cleanup 取消 signal、计时器和监听器，并允许重复清理", async (t) => {
  let calls = 0;
  let signal;
  const stop = start(async (currentSignal) => { signal = currentSignal; calls += 1; });
  assert.equal(page.listenerCount, 1);
  assert.equal(browser.listenerCount, 1);
  await settle();
  stop();
  stop();
  assert.equal(signal.aborted, true);
  assert.equal(page.listenerCount, 0);
  assert.equal(browser.listenerCount, 0);
  focus();
  visibility(true);
  visibility(false);
  t.mock.timers.tick(20_000);
  assert.equal(calls, 1);
});

test("cleanup 后即使忽略取消的旧请求完成也不重建计时器", async (t) => {
  const pending = deferred();
  let calls = 0;
  let signal;
  const stop = start((currentSignal) => { signal = currentSignal; calls += 1; return pending.promise; });
  stop();
  assert.equal(signal.aborted, true);
  pending.resolve();
  await settle();
  focus();
  visibility(false);
  t.mock.timers.tick(20_000);
  assert.equal(calls, 1);
});

test("拒绝与同步抛错均被接住，后续轮询可以继续", async (t) => {
  let calls = 0;
  start(() => {
    calls += 1;
    if (calls === 1) return Promise.reject(new Error("业务请求失败"));
    if (calls === 2) throw new Error("同步异常");
    return Promise.resolve();
  });
  await settle();
  t.mock.timers.tick(2_000);
  await settle();
  t.mock.timers.tick(2_000);
  assert.equal(calls, 3);
});

test("cleanup 引发 AbortError 拒绝不会泄漏或重新轮询", async (t) => {
  let calls = 0;
  const stop = start((signal) => {
    calls += 1;
    return new Promise((_resolve, reject) => {
      signal.addEventListener("abort", () => reject(new DOMException("cancelled", "AbortError")), { once: true });
    });
  });
  stop();
  await settle();
  t.mock.timers.tick(20_000);
  assert.equal(calls, 1);
});

test("清理后重新启动隔离 signal，旧异步完成不干扰新调度", async (t) => {
  const oldPending = deferred();
  let oldCalls = 0;
  let oldSignal;
  const stop = start((signal) => { oldSignal = signal; oldCalls += 1; return oldPending.promise; });
  stop();
  let newCalls = 0;
  let newSignal;
  start(async (signal) => { newSignal = signal; newCalls += 1; });
  oldPending.resolve();
  await settle();
  assert.notEqual(oldSignal, newSignal);
  assert.equal(oldSignal.aborted, true);
  assert.equal(newSignal.aborted, false);
  assert.equal(page.listenerCount, 1);
  assert.equal(browser.listenerCount, 1);
  t.mock.timers.tick(2_000);
  assert.equal(oldCalls, 1);
  assert.equal(newCalls, 2);
});
