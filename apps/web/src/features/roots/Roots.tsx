import { useEffect, useState } from "react";
import { ApiError, listManagedRoots, registerRoot, type ManagedRoot, type ManagedRootList, type RootCollectionState } from "@vaultsyncd/api-client";
import { Button, EmptyState, StatusBadge, Table } from "@vaultsyncd/ui";
import { useAuth } from "../auth/Auth";
import { SignedOut } from "../../services/auth";

const collectionLabels: Record<RootCollectionState, string> = {
  not_scanned: "尚未扫描", scanning: "正在采集", pending: "等待稳定或重试", complete: "本进程初始采集完成", blocked: "采集受阻",
};
export function registrationError(error: unknown): string {
  if (!(error instanceof ApiError)) return "无法确认注册结果。请先刷新清单核对路径，再决定是否重试；不要把网络失败当作未注册。";
  const reason = error.body.details?.reason ?? error.body.code;
  const steps: Record<string, string> = {
    outside_allowed_bases: "请选择下方允许基目录内的服务器绝对路径；账号无管理权限时请联系 owner。",
    root_overlap: "使用与既有 Root 无父子或别名重叠的独立目录。",
    registered_overlap: "使用与既有 Root 无父子或别名重叠的独立目录。",
    probe_failed: "核对目录存在、服务账号读取权限和挂载，再刷新重试。",
  };
  const next = steps[reason] ?? "请检查目录是本地 btrfs、已启用 CoW，且没有嵌套挂载、子卷或内部状态重叠；按服务端原因修复后再试。";
  return `${error.body.message}（${reason}） ${next} 请求编号：${error.body.request_id}`;
}
function Collection({ root }: { root: ManagedRoot }) {
  return <><StatusBadge tone={root.collection.state === "blocked" ? "warning" : "info"}>{collectionLabels[root.collection.state]}</StatusBadge>
    <p>最近一轮检查 {root.collection.examined} 项 · 待处理 {root.collection.pending} 项</p><p>{root.collection.detail}</p></>;
}
export function RootsPanel() {
  const auth = useAuth();
  const [data, setData] = useState<ManagedRootList | null>(null);
  const [loading, setLoading] = useState(false);
  const [registering, setRegistering] = useState(false);
  const [error, setError] = useState("");
  const [result, setResult] = useState("");
  const [revision, setRevision] = useState(0);
  const client = auth.client;
  const user = auth.account?.user_id;
  useEffect(() => {
    setData(null); setError("");
    if (!client || !user) return;
    let active = true;
    const controller = new AbortController();
    setLoading(true);
    client.request(options => listManagedRoots(options, { signal: controller.signal })).then(value => {
      if (active) setData(value);
    }).catch(cause => {
      if (!active) return;
      if (cause instanceof SignedOut) auth.reportSignedOut();
      setError(cause instanceof Error ? cause.message : "读取失败，请刷新重试。");
    }).finally(() => { if (active) setLoading(false); });
    return () => { active = false; controller.abort(); };
  }, [client, user, revision]);
  async function register(event: React.FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!client || registering || !data) return;
    const path = String(new FormData(event.currentTarget).get("path"));
    setRegistering(true); setError(""); setResult("");
    try {
      const registered = await client.request(options => registerRoot(options, { path }));
      setResult(`已注册 Root ${registered.root_id}；等待初始采集，现有文件保留。`);
      setRevision(value => value + 1);
    } catch (cause) {
      if (cause instanceof SignedOut) auth.reportSignedOut();
      setError(registrationError(cause));
    } finally { setRegistering(false); }
  }
  if (!auth.account) return <EmptyState title="请先登录">从账号与会话页面登录 owner 后查看 Root 配置与健康。</EmptyState>;
  return <><section className="vs-card"><h2>Root 健康与采集</h2>
    <p>现场核对挂载与目录身份。采集进度只代表本次服务进程的观察结果，不等于客户端已同步。</p>
    <Button disabled={loading || registering} onClick={() => setRevision(value => value + 1)}>刷新目录状态</Button>
    {loading && <p role="status">正在核验目录与采集状态…</p>}
    {error && <p role="alert">{error}</p>}{result && <p role="status">{result}</p>}
    {!loading && !data && !error && <p>尚未读取目录，不能判断为空或已同步。</p>}
    {data && <><h3>允许基目录</h3>{data.allowed_bases.length ? <ul>{data.allowed_bases.map(base => <li key={base}><code>{base}</code></li>)}</ul> : <p>未配置允许基目录，不能注册。请管理员设置服务端 roots.allowed_bases。</p>}
      <form onSubmit={event => void register(event)}><label>服务器目录绝对路径<input name="path" required maxLength={4096} placeholder="从允许基目录中选择目录" disabled={registering} /></label>
        <Button type="submit" disabled={registering || loading || !data.allowed_bases.length}>{registering ? "校验并注册中…" : "校验并注册 Root"}</Button>
        <p>目录必须已存在；校验失败不会清空或重写文件。</p></form></>}
  </section>
  {data && <Table caption="已注册 Root" rows={data.roots} rowKey={root => root.root_id} emptyMessage="尚未注册 Root；这不表示服务器目录为空或客户端已同步。" columns={[
    { key: "path", title: "服务器路径 / 身份", render: root => <><code>{root.path}</code><p><small>{root.root_id}</small></p></> },
    { key: "health", title: "现场健康", render: root => <><StatusBadge tone={root.health === "unavailable" ? "danger" : "info"}>{root.health === "healthy" ? "挂载核对通过" : "Root 不可用"}</StatusBadge>{root.reason && <p role="alert">{root.reason}</p>}<p>{root.next_step}</p></> },
    { key: "collection", title: "采集进度", render: root => <Collection root={root} /> },
    { key: "seq", title: "已提交序号", render: root => root.root_seq },
  ]} />}</>;
}
