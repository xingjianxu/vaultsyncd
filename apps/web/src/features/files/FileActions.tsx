import { createContext, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { Alert, Button, Group, Stack, Text } from "@mantine/core";
import type { ClientOptions, SnapshotEntry } from "@vaultsyncd/api-client";
import { Dialog } from "../../components/ui";
import { showFeedback } from "../../components/Feedback";
import { useAuth } from "../auth/Auth";
import { BrowserAuth, SignedOut } from "../../services/auth";
import type { DirectorySnapshot } from "./browser";
import { createClipboard, createFileActionRecovery, createUploadJob, createPasteJob, prepareDelete, type RecoverableOperation, type RecoveredOperation, type PreparedDelete, type FileActionJob, type FileClipboard } from "./actions";

export interface ActionDirectory { rootId: string; snapshot: DirectorySnapshot; parent: SnapshotEntry }
type Actions = {
  clipboard: FileClipboard | null; busy: boolean; locked: boolean; revision: number;
  stage: (context: ActionDirectory, entry: SnapshotEntry, mode: "copy" | "cut") => void;
  upload: (context: ActionDirectory, files: File[]) => void;
  paste: (context: ActionDirectory) => void;
  remove: (context: ActionDirectory, entry: SnapshotEntry) => void;
};
const Context = createContext<Actions | null>(null);
export function useFileActions() { return useContext(Context)!; }

class FileActionPaused extends SignedOut {
  constructor() { super(); this.message = "账号已切换，文件操作已暂停；请用原账号核对未确认记录。"; }
}

/** 显式认证动作或 Provider 卸载后，不允许旧任务继续产生下一条请求。 */
class ActionAuth extends BrowserAuth {
  private expectedGeneration: number;
  constructor(private source: BrowserAuth, private active: () => boolean) { super(); this.expectedGeneration = source.sessionGeneration; }
  resume() { this.expectedGeneration = this.source.sessionGeneration; }
  override request<T>(operation: (options: ClientOptions) => Promise<T>): Promise<T> {
    return this.source.request(options => {
      if (!this.active() || this.expectedGeneration !== this.source.sessionGeneration) throw new FileActionPaused();
      return operation(options);
    });
  }
}

/** 记录放在 Root/目录导航之上，切换目录不会遗失剪贴板或未确认的 OpId。 */
export function FileActionsProvider({ children }: { children: ReactNode }) {
  const auth = useAuth();
  const mounted = useRef(true);
  useEffect(() => { mounted.current = true; return () => { mounted.current = false; }; }, []);
  const client = useMemo(() => auth.client ? new ActionAuth(auth.client, () => mounted.current) : null, [auth.client]);
  const recoverySetup = useMemo(() => {
    if (!auth.account) return {};
    try {
      const recovery = createFileActionRecovery(auth.account.user_id);
      return { recovery, records: recovery.list() };
    } catch (cause) { return { error: cause instanceof Error ? cause.message : "无法读取文件操作恢复记录。" }; }
  }, [auth.account?.user_id]);
  const recovery = recoverySetup.recovery;
  const [records, setRecords] = useState<RecoverableOperation[]>(recoverySetup.records ?? []);
  const [recoveryError, setRecoveryError] = useState(recoverySetup.error ?? "");
  const [missing, setMissing] = useState<RecoveredOperation[]>([]);
  const [replay, setReplay] = useState<RecoverableOperation | null>(null);
  const [clipboard, setClipboard] = useState<FileClipboard | null>(null);
  const [job, setJob] = useState<FileActionJob | null>(null);
  const [busy, setBusy] = useState(false);
  const running = useRef(false);
  const [message, setMessage] = useState("");
  const [revision, setRevision] = useState(0);
  const [deletion, setDeletion] = useState<PreparedDelete | null>(null);
  useEffect(() => {
    if (!job) return;
    const warn = (event: BeforeUnloadEvent) => { event.preventDefault(); event.returnValue = ""; };
    window.addEventListener("beforeunload", warn);
    return () => window.removeEventListener("beforeunload", warn);
  }, [job]);
  const locked = auth.busy || busy || !!job || !!deletion || !!replay || records.length > 0 || !!recoveryError;
  function syncRecords() {
    try { setRecords(recovery?.list() ?? []); }
    catch (cause) { setRecoveryError(cause instanceof Error ? cause.message : "恢复日志读取失败。"); }
  }
  async function recover(opId?: string) {
    if (!client || !recovery || running.current || auth.busy) return;
    client.resume(); running.current = true; setBusy(true); setMissing([]);
    try {
      const results = opId ? [await recovery.resume(client, opId)] : await recovery.query(client);
      setMissing(results.filter(item => item.state === "missing"));
      for (const item of results) showFeedback(item.state === "committed" ? "原操作已提交；未执行的后续任务不会自动继续，请核对目录。"
        : item.state === "rejected" ? `原操作已拒绝：${item.result?.error_message ?? item.result?.error_code ?? "请刷新核对"}`
        : item.state === "pending" ? "原操作仍在处理中，尚未确认完成。" : "服务器尚未记录原操作。可核对原始请求后，明确选择重试；不会自动重新提交。", item.state === "committed" ? "green" : "yellow");
    } catch (cause) { failure(cause); }
    finally { running.current = false; setBusy(false); syncRecords(); setRevision(value => value + 1); }
  }
  function failure(cause: unknown) {
    if (cause instanceof SignedOut && !(cause instanceof FileActionPaused) && mounted.current) auth.reportSignedOut();
    const text = cause instanceof Error ? cause.message : "文件操作失败。";
    if (mounted.current) showFeedback(text, "red");
    return text;
  }
  async function run(next: FileActionJob) {
    if (running.current || auth.busy || !auth.account) return;
    client?.resume();
    running.current = true; setBusy(true); setJob(next); setMessage("");
    try {
      await next.run();
      if (next.state === "completed") {
        showFeedback(next.summary);
        if (next.label === "移动") setClipboard(null);
        setJob(null);
      } else { setMessage(next.summary); showFeedback(next.summary, "yellow"); }
    } catch (cause) { failure(cause); setMessage(next.summary); }
    finally { running.current = false; setBusy(false); syncRecords(); setRevision(value => value + 1); }
  }
  function start(factory: () => FileActionJob) {
    if (locked || running.current) return;
    try { void run(factory()); } catch (cause) { failure(cause); }
  }
  const actions: Actions = {
    clipboard, busy, locked, revision,
    stage(context, entry, mode) {
      if (locked) return;
      try { setClipboard(createClipboard(mode, context.rootId, context.snapshot, entry)); }
      catch (cause) { failure(cause); return; }
      showFeedback(mode === "copy" ? `已复制 ${entry.name}，请选择目标目录粘贴。` : `已剪切 ${entry.name}，粘贴后才移动。`);
    },
    upload: (context, files) => start(() => createUploadJob(client!, context.rootId, context.snapshot, context.parent, files, recovery)),
    paste: context => { if (clipboard) start(() => createPasteJob(client!, clipboard, context.rootId, context.snapshot, context.parent, recovery)); },
    async remove(context, entry) {
      if (locked || running.current) return;
      client?.resume();
      running.current = true; setBusy(true);
      try {
        const prepared = await prepareDelete(client!, context.rootId, context.snapshot, entry, recovery);
        setDeletion(prepared);
      } catch (cause) { failure(cause); }
      finally { running.current = false; setBusy(false); }
    },
  };
  return <Context.Provider value={actions}>
    {children}
    {job && <Alert color={message ? "yellow" : "blue"} title="文件操作" role="status">
      <Stack gap="xs">
        <Text size="sm">{busy ? `${job.label}正在处理，请勿关闭页面…` : message}</Text>
        {!busy && <Group><Button size="xs" disabled={auth.busy} onClick={() => void run(job)}>继续操作</Button>
          {job.canDiscard && <Button size="xs" variant="default" onClick={() => { showFeedback(`已结束后续操作。${job.progress.completed}/${job.progress.total} 项已提交，已完成内容保留。`, "yellow"); setJob(null); setMessage(""); syncRecords(); }}>结束操作</Button>}
        </Group>}
      </Stack>
    </Alert>}
    {recoveryError && <Alert color="red" title="文件操作恢复记录不可用">{recoveryError}</Alert>}
    {!job && records.length > 0 && <Alert color="yellow" title="未确认文件操作">
      <Stack gap="xs"><Text size="sm">有未确认的提交，请先核对原操作。后续批量任务不会自动恢复。</Text>
        {records.map(record => <Text size="xs" key={record.request.op_id}>{record.request.kind} · {record.request.name ?? record.request.entry_id} · Root {record.root} · Op {record.request.op_id}</Text>)}
        <Group><Button size="xs" disabled={busy || auth.busy} onClick={() => void recover()}>核对未确认操作</Button>
          {missing.map(record => <Button key={record.request.op_id} size="xs" variant="default" onClick={() => setReplay(record)}>重试原操作</Button>)}
        </Group>
      </Stack>
    </Alert>}
    <Dialog opened={!!replay} title="重试原操作确认" onClose={() => setReplay(null)}
      footer={replay && <Group justify="flex-end"><Button variant="default" onClick={() => setReplay(null)}>取消</Button>
        <Button color={replay.request.kind === "delete" ? "red" : "blue"} onClick={() => { const id = replay.request.op_id; setReplay(null); void recover(id); }}>继续原操作</Button></Group>}>
      {replay && <><Text>将按原始基线重试 {replay.request.kind}：{replay.request.name ?? replay.request.entry_id}。不会更新版本或扩大删除范围。</Text>
        <Text size="xs">Root {replay.root} · Op {replay.request.op_id}</Text></>}
    </Dialog>
    <Dialog opened={!!deletion} title="删除确认" onClose={() => setDeletion(null)}
      footer={deletion && <Group justify="flex-end"><Button variant="default" onClick={() => setDeletion(null)}>取消</Button>
        <Button color="red" onClick={() => { const next = deletion.createJob(); setDeletion(null); void run(next); }}>删除</Button></Group>}>
      {deletion && <Text>将“{deletion.entry.name}”{deletion.entry.kind === "directory" ? `及其所有子项（共 ${deletion.count} 项）` : ""}移入回收站？</Text>}
    </Dialog>
  </Context.Provider>;
}
