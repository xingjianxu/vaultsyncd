import { useEffect, useId } from "react";
import { notifications } from "@mantine/notifications";

type FeedbackColor = "red" | "green" | "blue" | "yellow";

/** 操作反馈统一入口；错误留到手动关闭，成功四秒后消失。 */
export function showFeedback(message: string, color: FeedbackColor = "green", id?: string) {
  return notifications.show({
    id, message, color, withBorder: true,
    autoClose: color === "green" || color === "blue" ? 4000 : false,
    role: color === "red" ? "alert" : "status",
    closeButtonProps: { "aria-label": "关闭通知" },
  });
}

/** 将页面已有的反馈状态投递到全局通知区，页面离开/状态清除时移除，避免旧错误跨页面残留。 */
export function Feedback({ message, color = "red" }: { message: string; color?: FeedbackColor }) {
  const id = useId();
  useEffect(() => {
    if (!message) return;
    showFeedback(message, color, id);
    return () => { notifications.hide(id); };
  }, [message, color, id]);
  return null;
}
