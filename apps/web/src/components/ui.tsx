import type { ReactNode } from "react";
import { Badge, Button, Card, Group, Modal, Stack, Table as MantineTable, Text, Title } from "@mantine/core";

export function EmptyState({ title, children }: { title: string; children: ReactNode }) {
  return <Card withBorder><Stack align="center" gap="sm" py="md"><Title order={2} size="h3">{title}</Title><div>{children}</div></Stack></Card>;
}
type Tone = "neutral" | "info" | "success" | "warning" | "danger";
const colors = { neutral: "gray", info: "blue", success: "green", warning: "yellow", danger: "red" } as const;
export function StatusBadge({ tone = "neutral", children }: { tone?: Tone; children: ReactNode }) {
  return <Badge color={colors[tone]} variant="light" className="status-badge" data-tone={tone}>{children}</Badge>;
}
export interface TableColumn<T> { readonly key: string; readonly title: string; readonly render: (row: T) => ReactNode }
export function Table<T>({ caption, columns, rows, rowKey, emptyMessage }: {
  caption: string; columns: readonly TableColumn<T>[]; rows: readonly T[];
  rowKey: (row: T) => string; emptyMessage: string;
}) {
  return <Card component="section" withBorder padding={0} aria-label={caption}>
    <Group justify="space-between" p="md"><Title order={3} size="h4">{caption}</Title><Text size="xs" c="dimmed">本页 {rows.length} 项</Text></Group>
    <MantineTable.ScrollContainer minWidth={640}>
      <MantineTable aria-label={caption} highlightOnHover verticalSpacing="sm" horizontalSpacing="md" className="data-table">
        <MantineTable.Thead><MantineTable.Tr>{columns.map(column => <MantineTable.Th key={column.key}>{column.title}</MantineTable.Th>)}</MantineTable.Tr></MantineTable.Thead>
        <MantineTable.Tbody>{rows.length ? rows.map(row => <MantineTable.Tr key={rowKey(row)}>{columns.map(column => <MantineTable.Td key={column.key}>{column.render(row)}</MantineTable.Td>)}</MantineTable.Tr>) : <MantineTable.Tr><MantineTable.Td colSpan={columns.length}><Text ta="center" c="dimmed" py="md">{emptyMessage}</Text></MantineTable.Td></MantineTable.Tr>}</MantineTable.Tbody>
      </MantineTable>
    </MantineTable.ScrollContainer>
  </Card>;
}
export function Dialog({ opened, title, children, onClose, footer }: { opened: boolean; title: string; children: ReactNode; onClose: () => void; footer?: ReactNode }) {
  return <Modal opened={opened} onClose={onClose} title={title} centered>
    <Stack>{children}{footer ?? <Group justify="flex-end"><Button data-autofocus variant="default" onClick={onClose}>关闭</Button></Group>}</Stack>
  </Modal>;
}
