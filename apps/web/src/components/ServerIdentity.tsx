import { useEffect, useState } from "react";
import { Button, Loader, Stack, Textarea } from "@mantine/core";
import { readServerIdentity } from "../services/server";

export function ServerIdentity() {
  const [attempt, setAttempt] = useState(0);
  const [serverId, setServerId] = useState("");
  const [error, setError] = useState<string>();
  const [loading, setLoading] = useState(true);
  useEffect(() => {
    const controller = new AbortController();
    setLoading(true); setServerId(""); setError(undefined);
    void readServerIdentity(undefined, controller.signal).then(id => {
      if (!controller.signal.aborted) setServerId(id);
    }).catch(cause => {
      if (!controller.signal.aborted) setError(cause instanceof Error ? cause.message : "无法读取 Server ID。");
    }).finally(() => {
      if (!controller.signal.aborted) setLoading(false);
    });
    return () => controller.abort();
  }, [attempt]);
  return <Stack gap="xs">
    <Textarea label="Server ID" value={serverId} readOnly autosize minRows={1}
      placeholder={loading ? "正在读取…" : "未读取"} error={error}
      aria-busy={loading} rightSection={loading ? <Loader size="xs" /> : undefined}
      styles={{ input: { fontFamily: "var(--mantine-font-family-monospace)" } }} />
    {error && <Button variant="default" size="xs" style={{ alignSelf: "flex-start" }} onClick={() => setAttempt(value => value + 1)}>重新读取 Server ID</Button>}
  </Stack>;
}
