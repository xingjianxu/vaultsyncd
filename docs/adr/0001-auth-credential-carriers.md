# ADR 0001 — 会话凭据的两种载体：浏览器 cookie 与 agent bearer

- 状态：已接受（S043）
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
2. **登录响应同时给出两样东西。** `POST /v1/auth/login` 的响应体带一对凭据（供 agent 直接取用），
   同时下发两个 cookie（供浏览器）。理由：不引入“客户端类型”请求头这类协议发明；两种调用者都能用
   自己需要的那一半。代价是浏览器可以在登录瞬间读到响应体里的凭据——这并不削弱同源隔离，因为能读到
   它的脚本本来就已经运行在页面里。
3. **刷新接受两种来源。** 请求体（agent）或 `vaultsyncd_refresh` cookie（浏览器，此时请求体可省略）。
4. **CSRF 只针对 cookie 载体。** 带 cookie 的写请求必须有同源证据：`Origin` 的主机部分与 `Host` 相同，
   或者 `Sec-Fetch-Site` 为 `same-origin`/`none`。两者都没有时按非浏览器客户端处理（放行）——它们不带
   环境凭据。判定失败一律 403 `ACCESS_DENIED`，不区分“跨源”与“缺头”。
5. **Bearer 请求不做 CSRF 判定。** 没有环境凭据就没有可被借用的身份；要求 agent 提供 `Origin` 只会
   逼出伪造值。
6. **agent 侧保管仍是客户端责任（未在本卡实现）。** GUI 用 OS 凭据库、headless 用受限权限文件的落地
   属于客户端卡；本 ADR 只固定“服务端接受 bearer、不接受把访问凭据写进 URL 或前端存储”的边界。

## 后果

- 会话失效、到期、轮换、注销对两种载体完全一致，没有两套状态。
- 部署必须在 TLS 终止处正确传递 `Host`（design §17.2 已要求不盲目信任 `X-Forwarded-*`）；反向代理改写
  主机名会让同源判定失败，表现为 403 而不是静默放行。这是有意选择（fail closed）。
- 浏览器刷新走 cookie 时请求体可省略，而 OpenAPI 把 `POST /v1/auth/refresh` 的请求体声明为必填：契约
  描述的是 agent 载体，浏览器这条路径由本 ADR 说明。若要严格统一，需要在契约里为刷新加一个“无请求体”
  的变体，属于后续契约工作。
- `Secure` 是固定属性，本地明文 HTTP 调试需要显式改用测试配置（design §17.2 不允许把明文公共 HTTP
  当常规方案）。
