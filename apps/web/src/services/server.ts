import { getHealth, type ClientOptions, type HealthResponse } from "@vaultsyncd/api-client";

// 同源部署；可在测试中注入传输，不存会话令牌。健康状态不能推导文件同步状态。
export function checkServer(options: ClientOptions = {
  base_url: window.location.origin,
  fetch: (url, init) => window.fetch(url, init),
  request_id: () => window.crypto.randomUUID(),
}, signal?: AbortSignal): Promise<HealthResponse> {
  return getHealth(options, { signal });
}
