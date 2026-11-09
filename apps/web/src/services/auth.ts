import { ApiError, browserLogin, browserRefresh, changePassword, getAccount, logout, revokeAllSessions, type AccountInfo, type ClientOptions } from "@vaultsyncd/api-client";

export class SignedOut extends Error {
  constructor() { super("会话已失效，请重新登录。"); }
}

/** 旧请求不能回写新会话，也不能被消费者误判为新会话退出。 */
class SessionChanged extends Error {
  constructor() { super("会话已变更，已停止旧请求。"); }
}

/** 浏览器只接触会话元信息。所有凭据由同源 HttpOnly cookie 携带。 */
export class BrowserAuth {
  private refreshing: { generation: number; promise: Promise<void> } | null = null;
  private generation = 0;
  /** 显式认证动作开始即暂停旧文件任务；普通 cookie refresh 不改变此代际。 */
  get sessionGeneration() { return this.generation; }
  private readonly options: ClientOptions;
  constructor(options?: ClientOptions, private readonly onSignedOut?: (cause?: SignedOut) => void) {
    const origin = options?.base_url ?? window.location.origin;
    const transport = options?.fetch ?? ((url, init) => window.fetch(url, init));
    this.options = {
      base_url: origin,
      request_id: options?.request_id ?? (() => window.crypto.randomUUID()),
      fetch: (url, init) => {
        if (new URL(url).origin !== new URL(origin).origin) throw new Error("拒绝跨源账号请求。");
        return transport(url, { ...init, credentials: "same-origin", cache: "no-store" });
      },
    };
  }
  private assertCurrent(generation: number) {
    if (generation !== this.generation) throw new SessionChanged();
  }
  private signedOut(generation: number): never {
    this.assertCurrent(generation);
    this.generation++;
    const cause = new SignedOut();
    this.onSignedOut?.(cause);
    throw cause;
  }
  async login(loginName: string, password: string): Promise<AccountInfo> {
    const generation = ++this.generation;
    try {
      await browserLogin(this.options, { login: loginName, password, device_label: "Web 浏览器" });
    } catch (cause) { this.assertCurrent(generation); throw cause; }
    this.assertCurrent(generation);
    return this.account();
  }
  /** 可供后续 Vault/历史/备份消费者复用；401 单飞刷新一次，只有最终 401 才退出。 */
  async request<T>(operation: (options: ClientOptions) => Promise<T>): Promise<T> {
    const generation = this.generation;
    try {
      const result = await operation(this.options);
      this.assertCurrent(generation);
      return result;
    } catch (error) {
      this.assertCurrent(generation);
      if (!(error instanceof ApiError) || error.status !== 401) throw error;
    }
    if (!this.refreshing || this.refreshing.generation !== generation) {
      const refresh = { generation, promise: Promise.resolve() };
      refresh.promise = browserRefresh(this.options).then(() => undefined).finally(() => {
        if (this.refreshing === refresh) this.refreshing = null;
      });
      this.refreshing = refresh;
    }
    try { await this.refreshing.promise; }
    catch (cause) {
      this.assertCurrent(generation);
      if (cause instanceof ApiError && cause.status === 401) this.signedOut(generation);
      throw cause;
    }
    this.assertCurrent(generation);
    try {
      const result = await operation(this.options);
      this.assertCurrent(generation);
      return result;
    } catch (cause) {
      this.assertCurrent(generation);
      if (cause instanceof ApiError && cause.status === 401) this.signedOut(generation);
      throw cause;
    }
  }
  account(): Promise<AccountInfo> { return this.request(getAccount); }
  async logout(): Promise<void> {
    this.generation++;
    await this.request((options) => logout(options));
    this.onSignedOut?.();
  }
  async changePassword(current: string, next: string): Promise<void> {
    this.generation++;
    await this.request((options) => changePassword(options, { current_password: current, new_password: next }));
    this.onSignedOut?.();
  }
  async revokeAll(): Promise<void> {
    this.generation++;
    await this.request(revokeAllSessions);
    this.onSignedOut?.();
  }
}
