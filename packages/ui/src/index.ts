// 共享展示层的唯一导出入口（docs/design.md §5.6）。
// 这里只允许无平台 I/O 的组件与类型：HTTP 客户端、本机 IPC 类型与生成代码分别属于
// packages/api-client、packages/local-ipc-types，不在此重导出。
export { StatusBadge } from "./StatusBadge";
export type { StatusBadgeProps, StatusTone } from "./StatusBadge";

export { Button, Dialog, EmptyState, Table } from "./components";
export type { TableColumn } from "./components";
export { AppProvider } from "./AppProvider";
export { useLiveRefresh } from "./useLiveRefresh";
export { PageShell } from "./PageShell";
export { Icon, TransferDirection } from "./Icons";
export { ActivityAction, activityOutcome } from "./ActivityAction";
export type { NavItem, PageShellProps } from "./PageShell";
