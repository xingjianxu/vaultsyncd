import { useState, useEffect } from "react";
import { Card, Group, Stack, Text, TextInput, Title } from "@mantine/core";
import { Button, Table } from "@vaultsyncd/ui";
import type { Problem, Notice } from "@vaultsyncd/local-ipc-types";
import type { AgentClient } from "../../services/agent";
import { NoticePanel } from "./NoticePanel";

export function Problems({ agent, run, busy, current, notices }: { agent: AgentClient; run(action: () => Promise<void>): void; busy: boolean; current?: Problem[]; notices?: Notice[] }) {
  const [items, setItems] = useState<Problem[]>(); const [search, setSearch] = useState("");
  useEffect(() => { if (current) setItems(current); }, [current]);
  async function load() { const result = await agent.listProblems(undefined, search || undefined); if (!result.problems) throw new Error("agent 未返回问题清单。"); setItems(result.problems); }
  return <Stack gap="lg">
    <Title order={1} size="h2">问题与恢复</Title>
    <Card withBorder>
      <Group align="flex-end">
        <TextInput className="desktop-grow" label="搜索路径或原因" value={search} onChange={event => setSearch(event.target.value)} />
        <Button disabled={busy} onClick={() => run(load)}>读取问题</Button>
      </Group>
    </Card>
    <NoticePanel agent={agent} notices={notices} run={run} busy={busy} />
    <Table<Problem> caption="真实持久问题" rows={(items ?? []).filter(problem => `${problem.path} ${problem.code}`.toLowerCase().includes(search.toLowerCase()))} rowKey={problem => `${problem.binding_id}:${problem.problem_id}`} emptyMessage={items ? "没有匹配的文件问题；Vault级问题见上方生命周期。" : "尚未读取问题。"} columns={[
      { key: "path", title: "路径", render: problem => problem.path ?? problem.entry_id ?? "未知路径" },
      { key: "reason", title: "原因", render: problem => <Stack gap={4}><Text size="sm">{problem.code}：{problem.reason}</Text><Text size="sm">本地内容：{problem.local_data}</Text><Text size="sm">下一步：{problem.next_step}</Text></Stack> },
      { key: "retry", title: "操作", render: problem => <Button size="xs" disabled={busy || !problem.retryable} onClick={() => run(async () => { await agent.retryProblem(problem.binding_id, problem.problem_id); await load(); })}>重试</Button> },
    ]} />
  </Stack>;
}
