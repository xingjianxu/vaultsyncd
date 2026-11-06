# ADR 0001 — 会话凭据的两种载体：浏览器 cookie 与 agent bearer

- 状态：已接受（S043；S077 收紧浏览器响应与缺失同源证据的 cookie 请求）
- 日期：2026-11-05
- 相关：design §15.2（`/v1/auth/*`）、§17.1（凭据形态）、§17.2（TLS 入口）、I15、T31

## 背景

design §17.1 要求访问凭据短期、刷新凭据可轮换、服务端只存必要摘要，并明确“GUI 不保存长期凭据到前端
localStorage；优先 OS 凭据库，headless 环境使用权限受限的专用凭据文件”。同一个服务端要同时服务两类
调用者，它们的威胁模型不同：

- **浏览器（基础 Web）**：凭据由浏览器保管，脚本可能被 XSS 影响，跨站请求会自动带上 cookie。
- **agent / CLI / GUI 的 Rust 侧**：可以自己保管凭据（OS 凭据库或受限权限文件），不会自动附带任何
  环境凭据，但可能把凭据写进日志或配置文件。

## 决定

1. **同一套会话、两种载体。** 服务端只有一张 `sessions` 表与一套摘要；载体只是“凭据怎么送”。
   - 浏览器：`vaultsyncd_session`（访问，`Path=/`）与 `vaultsyncd_refresh`（刷新，`Path=/v1/auth`），
     均为 `HttpOnly; Secure; SameSite=Strict`。
   - agent：`Authorization: Bearer <access token>`，刷新凭据放请求体 `POST /v1/auth/refresh`。
2. **浏览器与 agent 使用明确的响应契约。** agent 继续调用 `POST /v1/auth/login` 与请求体带刷新凭据的 `POST /v1/auth/refresh`，响应含凭据对。Web 使用 `POST /v1/auth/browser/login|refresh`，只返回会话身份与到期时间；令牌仅写入 HttpOnly cookie。这样浏览器脚本不能借刷新 cookie 从旧 agent 端点读出令牌正文。
3. **刷新来源与响应一致。** agent 端点必须在请求体带刷新凭据；浏览器端点无请求体，只取 cookie。两者复用同一轮换用例与会话表。
4. **Cookie 请求必须有同源证据。** Origin 的 HTTPS authority 与 Host 相同；显式 HTTP 例外仅限本机 localhost/127.0.0.1 测试入口。Origin 不得含路径、凭据、查询或片段。缺 Origin 时只接受 Sec-Fetch-Site 为 same-origin/none；带 Cookie 且两种证据都缺失时拒绝。跨源或缺证据均为 403 ACCESS_DENIED。
5. **无环境凭据的 native 请求保持兼容。** 无 Cookie、Origin 与 Sec-Fetch-Site 的 agent/CLI 请求无需伪造浏览器头。若带 Origin 则同样拒绝显式跨源请求。
6. **agent 侧保管仍是客户端责任（未在本卡实现）。** GUI 用 OS 凭据库、headless 用受限权限文件的落地
   属于客户端卡；本 ADR 只固定“服务端接受 bearer、不接受把访问凭据写进 URL 或前端存储”的边界。

## 后果

- 会话失效、到期、轮换、注销对两种载体完全一致，没有两套状态。
- 部署必须在 TLS 终止处正确传递 `Host`（design §17.2 已要求不盲目信任 `X-Forwarded-*`）；反向代理改写
  主机名会让同源判定失败，表现为 403 而不是静默放行。这是有意选择（fail closed）。
- S077 已消除浏览器刷新与 OpenAPI 必填请求体的不一致：browser/refresh 为独立无请求体端点，旧 refresh 按原契约只接受明文请求体凭据。
- `Secure` 是固定属性，本地明文 HTTP 调试需要显式改用测试配置（design §17.2 不允许把明文公共 HTTP
  当常规方案）。
