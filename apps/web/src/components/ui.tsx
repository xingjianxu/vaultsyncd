import type { ReactNode, ComponentProps } from "react";
import { Button as HeroButton } from "@heroui/react/button";
import { Card } from "@heroui/react/card";
import { Chip } from "@heroui/react/chip";
import { Modal } from "@heroui/react/modal";
import { Table as HeroTable } from "@heroui/react/table";

// The compatibility names keep feature actions explicit while HeroUI owns keyboard/press behavior.
type ButtonProps = Omit<ComponentProps<typeof HeroButton>, "onClick" | "onPress"> & { disabled?: boolean; onClick?: () => void };
export function Button({ disabled, onClick, type = "button", variant = "secondary", ...props }: ButtonProps) {
  return <HeroButton {...props} type={type} variant={variant} isDisabled={disabled || props.isDisabled} onPress={onClick} />;
}
export function EmptyState({ title, children }: { title: string; children: ReactNode }) {
  return <Card className="empty-state"><Card.Header><Card.Title render={props => <h2 {...props} />}>{title}</Card.Title></Card.Header><Card.Content><p>{children}</p></Card.Content></Card>;
}
type Tone = "neutral" | "info" | "success" | "warning" | "danger";
const colors = { neutral: "default", info: "accent", success: "success", warning: "warning", danger: "danger" } as const;
export function StatusBadge({ tone = "neutral", children }: { tone?: Tone; children: ReactNode }) {
  return <Chip size="sm" variant="soft" color={colors[tone]} data-tone={tone}><Chip.Label>{children}</Chip.Label></Chip>;
}
export interface TableColumn<T> { readonly key: string; readonly title: string; readonly render: (row: T) => ReactNode }
export function Table<T>({ caption, columns, rows, rowKey, emptyMessage }: {
  caption: string; columns: readonly TableColumn<T>[]; rows: readonly T[];
  rowKey: (row: T) => string; emptyMessage: string;
}) {
  return <section className="data-section" aria-label={caption}><h3 className="table-caption">{caption}</h3><HeroTable>
    <HeroTable.ScrollContainer><HeroTable.Content aria-label={caption} className="min-w-[640px]">
      <HeroTable.Header>{columns.map((column, i) => <HeroTable.Column key={column.key} id={column.key} isRowHeader={i === 0}>{column.title}</HeroTable.Column>)}</HeroTable.Header>
      <HeroTable.Body renderEmptyState={() => <div className="p-8 text-center text-muted">{emptyMessage}</div>}>
        {rows.map(row => <HeroTable.Row key={rowKey(row)} id={rowKey(row)}>{columns.map(column => <HeroTable.Cell key={column.key}>{column.render(row)}</HeroTable.Cell>)}</HeroTable.Row>)}
      </HeroTable.Body>
    </HeroTable.Content></HeroTable.ScrollContainer>
  </HeroTable></section>;
}
export function Dialog({ title, children, onClose }: { title: string; children: ReactNode; onClose: () => void }) {
  return <Modal isOpen onOpenChange={open => { if (!open) onClose(); }}><Modal.Backdrop><Modal.Container><Modal.Dialog>
    <Modal.Header><Modal.Heading>{title}</Modal.Heading></Modal.Header>
    <Modal.Body className="space-y-4">{children}</Modal.Body>
    <Modal.Footer><Button autoFocus onClick={onClose}>关闭</Button></Modal.Footer>
  </Modal.Dialog></Modal.Container></Modal.Backdrop></Modal>;
}
