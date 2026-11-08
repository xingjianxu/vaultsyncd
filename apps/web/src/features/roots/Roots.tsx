import { useEffect, useState } from "react";
import { ApiError, listManagedRoots, registerRoot, type ManagedRoot, type ManagedRootList, type RootCollectionState } from "@vaultsyncd/api-client";
import { Button } from "@heroui/react/button";
import { Card } from "@heroui/react/card";
import { Description } from "@heroui/react/description";
import { Input } from "@heroui/react/input";
import { Label } from "@heroui/react/label";
import { TextField } from "@heroui/react/textfield";
import { EmptyState, StatusBadge, Table } from "../../components/ui";
import { useAuth } from "../auth/Auth";
import { SignedOut } from "../../services/auth";

const collectionLabels: Record<RootCollectionState, string> = {
  not_scanned: "尚未扫描", scanning: "正在采集", pending: "等待稳定或重试", complete: "本进程初始采集完成", blocked: "采集受阻",
};
export function registrationError(error: unknown): string {
  if (!(error instanceof ApiError) || error.status >= 500 || error.status === 408) return "无法确认注册结果。请先刷新清单核对路径，再决定是否重试；不要把网络失败当作未注册。";
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
  return <div className="space-y-2"><StatusBadge tone={root.collection.state === "blocked" ? "warning" : "info"}>{collectionLabels[root.collection.state]}</StatusBadge>
    <p className="text-xs text-muted">最近一轮检查 {root.collection.examined} 项 · 待处理 {root.collection.pending} 项</p><p className="text-sm text-muted">{root.collection.detail}</p></div>;
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
  return <div className="space-y-4">
    <Card className="p-4 sm:p-5">
      <Card.Header className="gap-3">
        <div className="flex flex-wrap items-start justify-between gap-4">
          <div className="space-y-2"><Card.Title render={props => <h2 {...props} />}>Root 健康与采集</Card.Title>
            <Card.Description>现场核对挂载与目录身份。采集进度只代表本次服务进程的观察结果，不等于客户端已同步。</Card.Description></div>
          <Button size="sm" variant="secondary" isDisabled={loading || registering} onPress={() => setRevision(value => value + 1)}>刷新目录状态</Button>
        </div>
      </Card.Header>
      <Card.Content className="mt-4 space-y-3">
        {loading && <p role="status" className="text-sm text-muted">正在核验目录与采集状态…</p>}
        {error && <p role="alert" className="rounded-md bg-danger-soft p-4 text-sm text-danger">{error}</p>}
        {result && <p role="status" className="rounded-md bg-success-soft p-4 text-sm text-success">{result}</p>}
        {!loading && !data && !error && <p className="text-sm text-muted">尚未读取目录，不能判断为空或已同步。</p>}
      </Card.Content>
    </Card>
    {data && <>
      <div className="grid items-start gap-4 xl:grid-cols-[minmax(0,1.6fr)_minmax(16rem,1fr)]">
        <Card className="p-4 sm:p-5">
          <Card.Header><Card.Title render={props => <h2 {...props} />}>注册服务器目录</Card.Title>
            <Card.Description>将允许基目录中的已有目录加入管理。</Card.Description></Card.Header>
          <Card.Content className="mt-3">
            <form className="space-y-3" onSubmit={event => void register(event)}>
              <TextField name="path" isRequired isDisabled={registering} maxLength={4096}>
                <Label>服务器目录绝对路径</Label>
                <Input className="w-full font-mono" placeholder="从允许基目录中选择目录" />
                <Description>目录必须已存在；校验失败不会清空或重写文件。</Description>
              </TextField>
              <Button size="sm" type="submit" isDisabled={registering || loading || !data.allowed_bases.length}>{registering ? "校验并注册中…" : "校验并注册 Root"}</Button>
            </form>
          </Card.Content>
        </Card>
        <Card variant="secondary" className="p-4 sm:p-5">
          <Card.Header><Card.Title render={props => <h2 {...props} />}>允许基目录</Card.Title>
            <Card.Description>仅接受以下范围内的服务器路径。</Card.Description></Card.Header>
          <Card.Content className="mt-4">
            {data.allowed_bases.length ? <ul className="space-y-2">{data.allowed_bases.map(base => <li className="rounded-md bg-surface p-3 text-sm break-all" key={base}><code>{base}</code></li>)}</ul>
              : <p className="text-sm text-muted">未配置允许基目录，不能注册。请管理员设置服务端 roots.allowed_bases。</p>}
          </Card.Content>
        </Card>
      </div>
      <Table caption="已注册 Root" rows={data.roots} rowKey={root => root.root_id} emptyMessage="尚未注册 Root；这不表示服务器目录为空或客户端已同步。" columns={[
        { key: "path", title: "服务器路径 / 身份", render: root => <div className="space-y-2"><code className="text-sm break-all">{root.path}</code><p className="text-xs text-muted break-all">{root.root_id}</p></div> },
        { key: "health", title: "现场健康", render: root => <div className="space-y-2"><StatusBadge tone={root.health === "unavailable" ? "danger" : "success"}>{root.health === "healthy" ? "挂载核对通过" : "Root 不可用"}</StatusBadge>{root.reason && <p role="alert" className="text-sm text-danger">{root.reason}</p>}<p className="text-sm text-muted">{root.next_step}</p></div> },
        { key: "collection", title: "采集进度", render: root => <Collection root={root} /> },
        { key: "seq", title: "已提交序号", render: root => <span className="font-mono text-sm">{root.root_seq}</span> },
      ]} />
    </>}
  </div>;
}
