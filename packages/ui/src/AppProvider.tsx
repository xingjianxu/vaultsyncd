import type { ReactNode } from "react";
import { MantineProvider } from "@mantine/core";
import { Notifications } from "@mantine/notifications";

/** 两端共用 Mantine 默认主题；nonce 的取得由各自宿主决定。 */
export function AppProvider({ children, getStyleNonce }: { children: ReactNode; getStyleNonce?: () => string }) {
  return <MantineProvider defaultColorScheme="auto" getStyleNonce={getStyleNonce}>
    <Notifications position="top-right" limit={3} zIndex={1100} />
    {children}
  </MantineProvider>;
}
