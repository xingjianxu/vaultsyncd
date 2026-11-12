import { CloudDownload, CloudUpload, type LucideIcon, type LucideProps } from "lucide-react";

/** 图标的名称由外层控件提供，避免屏幕阅读器重复朗读。 */
export function Icon({ icon: Component, ...props }: LucideProps & { icon: LucideIcon }) {
  return <Component size={20} strokeWidth={1.6} className="app-icon" aria-hidden="true" {...props} />;
}

/** 传输方向是内容而非按钮，保留独立的可访问名称和悬停提示。 */
export function TransferDirection({ direction }: { direction: string }) {
  if (direction !== "upload" && direction !== "download") return <>{direction}</>;
  const label = direction === "upload" ? "上传" : "下载";
  return <span role="img" aria-label={label} title={label} style={{ display: "inline-flex", verticalAlign: "middle" }}>
    <Icon icon={direction === "upload" ? CloudUpload : CloudDownload} />
  </span>;
}
