import { ArchiveRestore, FolderPlus, History, Move, Trash2, type LucideIcon } from "lucide-react";
import { Icon, TransferDirection } from "./Icons";

type Activity = { direction: string; action: string; status: string };
const operations: Record<string, { label: string; result: string; icon: LucideIcon }> = {
  create_directory: { label: "新建目录", result: "目录已创建", icon: FolderPlus },
  move: { label: "移动/重命名", result: "移动/重命名已提交", icon: Move },
  delete: { label: "删除", result: "删除已提交", icon: Trash2 },
  restore_version: { label: "恢复历史版本", result: "历史版本已恢复", icon: History },
  restore_deleted: { label: "从回收站恢复", result: "回收站条目已恢复", icon: ArchiveRestore },
};

/** 两端使用相同动作名称；元数据操作不冒充文件上传。 */
export function ActivityAction({ direction, action }: Pick<Activity, "direction" | "action">) {
  if (direction !== "operation") return <TransferDirection direction={direction} />;
  const operation = Object.hasOwn(operations, action) ? operations[action] : undefined;
  if (!operation) return <>{action}</>;
  return <span role="img" aria-label={operation.label} title={operation.label} style={{ display: "inline-flex", verticalAlign: "middle" }}>
    <Icon icon={operation.icon} />
  </span>;
}

/** committed仅说明服务端提交；不推断客户端应用或落盘。 */
export function activityOutcome({ action, status }: Pick<Activity, "action" | "status">): string {
  if (status === "failed") return "失败";
  if (status === "committed") {
    if (action === "sync_commit") return "同步已提交";
    if (Object.hasOwn(operations, action)) return operations[action].result;
  }
  if (action === "upload_finalize" && status === "ready") return "上传内容已校验";
  if (action === "download" && status === "served") return "已提供下载内容";
  return `${action} · ${status}`;
}
