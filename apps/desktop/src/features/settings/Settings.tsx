import { useEffect, useRef, useState } from "react";
import { Alert, Button, Card, Group, Radio, Stack, Switch, Text, Title } from "@mantine/core";
import type { DesktopSettings, DesktopSettingsClient } from "../../services/desktop";

export function Settings({ client }: { client: DesktopSettingsClient }) {
  const [saved, setSaved] = useState<DesktopSettings>();
  const [draft, setDraft] = useState<DesktopSettings>();
  const [reading, setReading] = useState(true);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState("");
  const [feedback, setFeedback] = useState("");
  const [attempt, setAttempt] = useState(0);
  const mounted = useRef(false);
  const writing = useRef(false);
  useEffect(() => {
    mounted.current = true;
    let current = true;
    setReading(true); setError(""); setFeedback("");
    void client.get().then(value => {
      if (current) { setSaved(value); setDraft(value); }
    }).catch(error => {
      if (current) setError(error instanceof Error ? error.message : "无法读取程序配置。");
    }).finally(() => { if (current) setReading(false); });
    return () => { current = false; mounted.current = false; };
  }, [client, attempt]);
  const busy = reading || saving;
  const changed = !!draft && (!saved || draft.close_action !== saved.close_action || draft.stop_agent_on_exit !== saved.stop_agent_on_exit);
  async function save() {
    if (!draft || writing.current || !changed || reading) return;
    writing.current = true; setSaving(true); setError(""); setFeedback("");
    try {
      const value = await client.set(draft);
      if (mounted.current) { setSaved(value); setDraft(value); setFeedback("程序配置已保存。"); }
    } catch (error) {
      if (mounted.current) setError(error instanceof Error ? error.message : "无法保存程序配置。");
    } finally {
      writing.current = false;
      if (mounted.current) setSaving(false);
    }
  }
  function change(value: DesktopSettings) { setDraft(value); setFeedback(""); }
  return <Stack gap="lg">
    <Title order={1} size="h2">程序配置</Title>
    {error && <Alert color="red" role="alert">{error}</Alert>}
    <Card component="section" withBorder>
      <Stack>
        <Radio.Group label="关闭主窗口时" value={draft?.close_action ?? ""} onChange={value => {
          if (draft && (value === "tray" || value === "exit")) change({ ...draft, close_action: value });
        }}>
          <Stack gap="sm" mt="sm">
            <Radio value="tray" label="最小化到托盘" disabled={busy || !draft} />
            <Radio value="exit" label="完全退出程序" disabled={busy || !draft} />
          </Stack>
        </Radio.Group>
        <Switch label="退出程序时自动关闭 agent" checked={draft?.stop_agent_on_exit ?? true}
          disabled={busy || !draft || draft.close_action !== "exit"} onChange={event => {
            if (draft) change({ ...draft, stop_agent_on_exit: event.currentTarget.checked });
          }} />
        {draft?.close_action === "exit" && draft.stop_agent_on_exit && <Text size="sm" c="dimmed">关闭 agent 将停止同步；重启后需恢复已暂停的同步目录。</Text>}
        <Group>
          <Button loading={saving} disabled={busy || !changed} onClick={() => void save()}>保存程序配置</Button>
          {!draft && <>
            <Button variant="default" disabled={busy} onClick={() => setAttempt(value => value + 1)}>重新读取</Button>
            {!reading && error && <Button variant="default" onClick={() => {
              setDraft({ close_action: "tray", stop_agent_on_exit: true }); setError(""); setFeedback("");
            }}>使用默认配置</Button>}
          </>}
          <Text size="sm" role="status" data-settings-feedback>{reading ? "正在读取程序配置。" : feedback || (changed ? "有未保存的修改。" : "")}</Text>
        </Group>
      </Stack>
    </Card>
  </Stack>;
}
