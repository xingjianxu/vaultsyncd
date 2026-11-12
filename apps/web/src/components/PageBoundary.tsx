import { Component, createContext, lazy, useContext, type ComponentProps, type ComponentType, type LazyExoticComponent, type ReactNode } from "react";
import { Group, Loader, Text } from "@mantine/core";
import { EmptyState } from "./ui";

const RecoveryContext = createContext<object>({});

// Keep the lazy promise stable while Suspense retries an uncommitted mount,
// but recreate it when the boundary explicitly starts another recovery attempt.
export function recoverableLazy<T extends ComponentType<any>>(load: () => Promise<{ default: T }>) {
  const panels = new WeakMap<object, LazyExoticComponent<T>>();
  return function RecoverablePanel(props: ComponentProps<T>) {
    const generation = useContext(RecoveryContext);
    let Panel = panels.get(generation);
    if (!Panel) { Panel = lazy(load); panels.set(generation, Panel); }
    return <Panel {...props} />;
  };
}

const reloadKey = "vaultsyncd.page-recovery-reload";
const reloadInterval = 5 * 60_000;
const isModuleError = (error: unknown) => error instanceof Error && /ChunkLoadError|Loading chunk|Failed to fetch dynamically imported module|Importing a module script failed|error loading dynamically imported module/i.test(`${error.name}: ${error.message}`);

export class PageBoundary extends Component<{ children: ReactNode }, { failed: boolean }> {
  state = { failed: false };
  private timer: ReturnType<typeof setTimeout> | undefined;
  private failures = 0;
  private moduleError = false;
  private generation = {};
  static getDerivedStateFromError() { return { failed: true }; }

  componentDidMount() {
    window.addEventListener("online", this.resume);
    document.addEventListener("visibilitychange", this.resume);
  }
  componentDidCatch(error: unknown) {
    this.failures++;
    this.moduleError = isModuleError(error);
    this.schedule();
  }
  componentWillUnmount() {
    this.cancel();
    window.removeEventListener("online", this.resume);
    document.removeEventListener("visibilitychange", this.resume);
  }
  private cancel = () => { clearTimeout(this.timer); this.timer = undefined; };
  private available = () => document.visibilityState !== "hidden" && navigator.onLine !== false;
  private resume = () => {
    this.cancel();
    if (this.state.failed && this.available()) this.schedule();
  };
  private schedule() {
    this.cancel();
    if (this.available()) this.timer = setTimeout(this.retry, Math.min(2000 * 2 ** Math.min(this.failures - 1, 4), 30_000));
  }
  private retry = () => {
    this.cancel();
    if (!this.available()) return;
    // Browsers may also cache a failed module graph, or keep an old asset URL
    // after deployment. Only that failure needs an occasional document reload.
    if (this.moduleError && this.failures >= 2) {
      try {
        const last = Number(sessionStorage.getItem(reloadKey));
        const now = Date.now();
        if (!last || now - last >= reloadInterval) {
          sessionStorage.setItem(reloadKey, String(now));
          window.location.reload();
          return;
        }
      } catch { /* Without a persistent cooldown, keep recovery inside the page. */ }
    }
    this.generation = {};
    this.setState({ failed: false });
  };
  render() {
    if (this.state.failed) return <EmptyState title="管理页面加载失败"><Group role="status"><Loader size="sm" /><Text>正在自动重新连接…</Text></Group></EmptyState>;
    return <RecoveryContext.Provider value={this.generation}>{this.props.children}</RecoveryContext.Provider>;
  }
}
