import type { ReactNode } from "react";

/**
 * 徽章的视觉基调。它只描述展示语气，不是同步状态本身：状态与拒绝原因由 Rust 纯核心定义
 * （docs/design.md §6、§7），应用层把状态映射成 tone 后再渲染。
 */
export type StatusTone = "neutral" | "info" | "success" | "warning" | "danger";

export type StatusBadgeProps = {
  readonly tone?: StatusTone;
  readonly children: ReactNode;
};

/**
 * 纯展示组件：不读文件、不发请求、不持有状态，属于 docs/design.md §5.6 的共享展示层。
 * 文案由调用方（desktop/web）提供，因此同一状态在两个应用里可以有不同的措辞。
 */
export function StatusBadge({ tone = "neutral", children }: StatusBadgeProps) {
  return (
    <span className={`vs-status-badge vs-status-badge--${tone}`} data-tone={tone}>
      {children}
    </span>
  );
}
