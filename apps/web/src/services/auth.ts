import { ApiError, browserLogin, browserRefresh, changePassword, getAccount, logout, revokeAllSessions, type AccountInfo, type ClientOptions } from "@vaultsyncd/api-client";

export class SignedOut extends Error {
  constructor() { super("会话已失效，请重新登录。"); }
}

/** 浏览器只接触会话元信息。所有凭据由同源 HttpOnly cookie 携带。 */
export class BrowserAuth {
  private refreshing: Promise<void> | null = null;
  private readonly options: ClientOptions;
  constructor(options?: ClientOptions) {
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
  async login(loginName: string, password: string): Promise<AccountInfo> {
    await browserLogin(this.options, { login: loginName, password, device_label: "Web 浏览器" });
    return getAccount(this.options);
  }
  /** 可供后续 Root/历史/备份消费者复用；401 单飞刷新一次，失败回到登录。 */
  async request<T>(operation: (options: ClientOptions) => Promise<T>): Promise<T> {
    try { return await operation(this.options); }
    catch (error) {
      if (!(error instanceof ApiError) || error.status !== 401) throw error;
      if (!this.refreshing) {
        this.refreshing = browserRefresh(this.options).then(() => undefined).catch((cause) => {
          if (cause instanceof ApiError && cause.status === 401) throw new SignedOut();
          throw cause;
        }).finally(() => { this.refreshing = null; });
      }
      await this.refreshing;
      try { return await operation(this.options); }
      catch (cause) { if (cause instanceof ApiError && cause.status === 401) throw new SignedOut(); throw cause; }
    }
  }
  account(): Promise<AccountInfo> { return this.request(getAccount); }
  async logout(): Promise<void> { await this.request((options) => logout(options)); }
  async changePassword(current: string, next: string): Promise<void> {
    await this.request((options) => changePassword(options, { current_password: current, new_password: next }));
  }
  async revokeAll(): Promise<void> { await this.request(revokeAllSessions); }
}
