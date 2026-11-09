import type { ComponentPropsWithoutRef, ReactNode } from "react";
import { Button as MantineButton, Card, Group, Modal, Stack, Table as MantineTable, Text, Title, type ButtonProps } from "@mantine/core";

export function Button({ type = "button", ...props }: ButtonProps & Omit<ComponentPropsWithoutRef<"button">, keyof ButtonProps>) {
  return <MantineButton type={type} {...props} />;
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
        <MantineTable.Thead><MantineTable.Tr>{columns.map(column => <MantineTable.Th scope="col" key={column.key}>{column.title}</MantineTable.Th>)}</MantineTable.Tr></MantineTable.Thead>
        <MantineTable.Tbody>{rows.length ? rows.map(row => <MantineTable.Tr key={rowKey(row)}>{columns.map(column => <MantineTable.Td key={column.key}>{column.render(row)}</MantineTable.Td>)}</MantineTable.Tr>) : <MantineTable.Tr><MantineTable.Td colSpan={columns.length}><Text ta="center" c="dimmed" py="md">{emptyMessage}</Text></MantineTable.Td></MantineTable.Tr>}</MantineTable.Tbody>
      </MantineTable>
    </MantineTable.ScrollContainer>
  </Card>;
}

/** 保持组件挂载并控制 opened，使 Mantine 能恢复打开对话框前的焦点。 */
export function Dialog({ opened = true, title, children, onClose, footer }: {
  opened?: boolean; title: string; children: ReactNode; onClose: () => void; footer?: ReactNode;
}) {
  return <Modal opened={opened} onClose={onClose} title={title} centered>
    <Stack>{children}{footer ?? <Group justify="flex-end"><Button data-autofocus variant="default" onClick={onClose}>关闭</Button></Group>}</Stack>
  </Modal>;
}

export function EmptyState({ title, children }: { title: string; children: ReactNode }) {
  return <Card withBorder><Stack align="center" gap="sm" py="md"><Title order={2} size="h3">{title}</Title><div>{children}</div></Stack></Card>;
}
