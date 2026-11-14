import { useEffect, useState } from "react";
import { Alert, Card, Group, Stack, Text, Title } from "@mantine/core";
import { Button, StatusBadge, Table } from "@vaultsyncd/ui";
import type { Notice } from "@vaultsyncd/local-ipc-types";
import type { AgentClient, NotificationSettings } from "../../services/agent";

export function NoticePanel({ agent, notices, run, busy }: { agent: AgentClient; notices?: Notice[]; run(action: () => Promise<void>): void; busy: boolean }) {
  const [value, setValue] = useState<NotificationSettings>(); const [error, setError] = useState("");
  useEffect(() => {
    let stopped = false;
    async function read() {
      try { const settings = await agent.notificationSettings(); if (!stopped) { setValue(settings); setError(""); } }
      catch { if (!stopped) setError("通知设置不可用；问题仍由agent提供。"); }
    }
    void read(); const timer = window.setInterval(() => void read(), 2000);
    return () => { stopped = true; window.clearInterval(timer); };
  }, [agent]);
  return <Card component="section" withBorder>
    <Stack>
      <Group justify="space-between">
        <Title order={2} size="h3">系统通知与恢复状态</Title>
        <Button variant="default" disabled={busy || !value} onClick={() => run(async () => { setValue(await agent.setNotifications(!value!.enabled)); })}>{value?.enabled ? "关闭系统通知" : "开启系统通知"}</Button>
      </Group>
      <Text size="sm" role="status">{value ? `系统通知${value.enabled ? "已开启" : "已关闭"}；本次界面提交给Windows ${value.delivered} 条，失败或策略禁用 ${value.blocked} 条。` : "尚未读取通知设置。"}</Text>
      {(error || value?.last_error) && <Alert color="red" role="alert">{error || value?.last_error}</Alert>}
      <Text size="sm" c="dimmed">开启会注册开始菜单快捷方式；关闭或清除通知不会清理问题、待办或冲突副本。同一服务器、Vault 的同原因每个活动代际仅领取一次。</Text>
      <Table<Notice> caption="Vault问题生命周期" rows={notices ?? []} rowKey={notice => notice.key} emptyMessage={notices ? "未记录需处理的Vault问题。" : "尚未取得问题生命周期。"} columns={[
        { key: "server", title: "Server ID", render: notice => notice.server_id ?? "—" },
        { key: "vault", title: "Vault", render: notice => notice.vault_id ?? "全局" },
        { key: "cause", title: "原因", render: notice => notice.cause === "CONFLICT_COPY" ? "冲突副本待检查" : notice.cause },
        { key: "state", title: "状态", render: notice => <StatusBadge tone={notice.state === "resolved" ? "success" : "warning"}>{notice.state === "resolved" ? "已恢复" : `仍需处理 · 活动代际 ${notice.generation}`}</StatusBadge> },
        { key: "delivery", title: "系统投递", render: notice => notice.claimed ? "本代际已领取投递（不代表用户已读）" : "待前台领取" },
      ]} />
    </Stack>
  </Card>;
}
