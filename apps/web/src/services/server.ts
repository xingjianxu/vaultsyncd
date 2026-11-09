import { getCapabilities, getHealth, type ClientOptions, type HealthResponse } from "@vaultsyncd/api-client";

const sameOrigin = (): ClientOptions => ({
  base_url: window.location.origin,
  fetch: (url, init) => window.fetch(url, init),
  request_id: () => window.crypto.randomUUID(),
});

// 同源部署；可在测试中注入传输，不存会话令牌。健康状态不能推导文件同步状态。
export function checkServer(options: ClientOptions = sameOrigin(), signal?: AbortSignal): Promise<HealthResponse> {
  return getHealth(options, { signal });
}

export async function readServerIdentity(options: ClientOptions = sameOrigin(), signal?: AbortSignal): Promise<string> {
  return (await getCapabilities(options, { signal })).server_id;
}
