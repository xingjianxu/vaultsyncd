import { useEffect } from "react";

/** 首次前台加载，随后仅在页面可见时串行后台刷新；load 应自行处理业务错误。 */
export function useLiveRefresh(
  load: (signal: AbortSignal, background: boolean) => Promise<void>,
  enabled: boolean,
  interval = 2_000,
): void {
  useEffect(() => {
    if (enabled) return startLiveRefresh(load, interval);
  }, [load, enabled, interval]);
}

/** @internal 调度与 React 生命周期分离，便于使用虚拟时钟验证。 */
export function startLiveRefresh(
  load: (signal: AbortSignal, background: boolean) => Promise<void>,
  interval: number,
): () => void {
  const controller = new AbortController();
  let timer: ReturnType<typeof setTimeout> | undefined;
  let running = false;

  function clearTimer() {
    if (timer !== undefined) clearTimeout(timer);
    timer = undefined;
  }

  async function refresh(background: boolean) {
    if (controller.signal.aborted || running) return;
    clearTimer();
    running = true;
    try {
      await load(controller.signal, background);
    } catch {
      // load 负责呈现业务错误；取消或意外拒绝也不能形成未处理的 Promise。
    } finally {
      running = false;
      if (!controller.signal.aborted && !document.hidden) {
        timer = setTimeout(() => {
          timer = undefined;
          if (!document.hidden) void refresh(true);
        }, interval);
      }
    }
  }

  function resume() {
    if (!document.hidden) void refresh(true);
  }

  function visibilityChanged() {
    if (document.hidden) clearTimer();
    else resume();
  }

  document.addEventListener("visibilitychange", visibilityChanged);
  window.addEventListener("focus", resume);
  // 即使初始隐藏也完成首次加载，但不启动隐藏页面的轮询。
  void refresh(false);

  return () => {
    clearTimer();
    document.removeEventListener("visibilitychange", visibilityChanged);
    window.removeEventListener("focus", resume);
    controller.abort();
  };
}
