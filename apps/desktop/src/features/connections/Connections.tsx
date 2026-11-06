import { useState, type FormEvent } from "react";
import { Button } from "@vaultsyncd/ui";
import type { ConnectionInfo } from "@vaultsyncd/local-ipc-types";
import type { AgentClient } from "../../services/agent";

export function Connections({ agent, connection, changed, run, busy }: {
  agent: AgentClient; connection?: ConnectionInfo; changed(): Promise<void>;
  run(action: () => Promise<void>): void; busy: boolean;
}) {
  const [password, setPassword] = useState("");
  function configure(event: FormEvent<HTMLFormElement>) {
    event.preventDefault(); const form = new FormData(event.currentTarget);
    run(async () => {
      await agent.setConnection({ serverId: String(form.get("server_id")), serverUrl: String(form.get("server_url")), caFile: String(form.get("ca_file") || "") || undefined });
      await changed();
    });
  }
  function login(event: FormEvent<HTMLFormElement>) {
    event.preventDefault(); const form = new FormData(event.currentTarget);
    const username = String(form.get("username")); const secret = password;
    setPassword("");
    run(async () => { await agent.login(username, secret); await changed(); });
  }
  return <><div className="vs-eyebrow">CLIENT / CONNECTIONS</div><h1>连接设置</h1>
    <p>通过本机命名管道管理 agent。登录凭据由本机 agent 保存；关闭界面后同步继续。</p>
    <section className="vs-card"><h2>服务器</h2>
      <p>当前账号：{connection ? connection.authenticated ? "已登录（持有凭据，连接仍需核验）" : "未登录" : "尚未读取"}</p>
      <form onSubmit={configure} key={`${connection?.server_id}:${connection?.server_url}`}>
        <label>服务器地址<input required name="server_url" type="url" defaultValue={connection?.server_url} placeholder="https://sync.example.com" /></label>
        <label>服务器身份（管理员提供）<input required name="server_id" pattern="[0-9a-f]{32}" defaultValue={connection?.server_id} /></label>
        <details><summary>开发证书</summary><label>附加 CA 文件（本机绝对路径）<input name="ca_file" defaultValue={connection?.ca_file} /></label><p>仅添加明确选择的 CA，始终校验证书和服务器名称。</p></details>
        <Button type="submit" disabled={busy}>保存连接</Button>
      </form>
    </section>
    <section className="vs-card"><h2>账号</h2><form onSubmit={login} autoComplete="off">
      <label>账号<input required name="username" autoComplete="username" /></label>
      <label>密码<input required name="password" type="password" autoComplete="current-password" value={password} onChange={event => setPassword(event.target.value)} /></label>
      <Button type="submit" disabled={busy}>登录</Button>
      <Button type="button" disabled={busy || !connection?.authenticated} onClick={() => run(async () => { await agent.logout(); await changed(); })}>退出登录</Button>
    </form></section></>;
}
