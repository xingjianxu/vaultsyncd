import type { ReactNode } from "react";
import { ActionIcon, Anchor, AppShell, Breadcrumbs, Burger, Container, Divider, Group, NavLink, Text, useComputedColorScheme, useMantineColorScheme } from "@mantine/core";
import { useDisclosure } from "@mantine/hooks";

export interface NavItem { readonly path: string; readonly label: string; readonly icon?: ReactNode }
export interface PageShellProps {
  product: string;
  items: readonly NavItem[];
  activePath: string;
  status: ReactNode;
  children: ReactNode;
  homePath?: string;
  headerActions?: ReactNode;
  sidebarTitle?: string;
  sidebarFooter?: ReactNode;
  contentWidth?: string | number;
}

export function Icon({ path }: { path: string }) {
  return <svg className="app-icon" aria-hidden="true" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round"><path d={path} /></svg>;
}

/** 无路由或传输依赖：应用提供导航项、当前路径与真实连接状态。 */
export function PageShell({ product, items, activePath, status, children, homePath = items[0]?.path ?? "/", headerActions, sidebarTitle, sidebarFooter, contentWidth = "xl" }: PageShellProps) {
  const [opened, { toggle, close }] = useDisclosure();
  const { toggleColorScheme } = useMantineColorScheme();
  const colorScheme = useComputedColorScheme("light");
  const active = items.find(item => item.path === activePath);
  return <>
    <a className="skip-link" href="#main-content" onClick={event => { event.preventDefault(); document.getElementById("main-content")?.focus(); }}>跳到主要内容</a>
    <AppShell header={{ height: 48 }} navbar={{ width: 240, breakpoint: "sm", collapsed: { mobile: !opened } }} padding="md">
      <AppShell.Header className="app-header">
        <Group className="app-brand" h="100%" px="md" gap="sm" wrap="nowrap"><Burger opened={opened} onClick={toggle} hiddenFrom="sm" size="sm" aria-label="切换导航" aria-controls="main-navigation" /><Anchor href={`#${homePath}`} fw={700} c="inherit" underline="never">vaultsyncd</Anchor></Group>
        <Group className="app-topbar" h="100%" px="md" gap="sm" wrap="nowrap">
          <Breadcrumbs visibleFrom="sm" aria-label="页面位置"><Anchor href={`#${homePath}`} size="sm">{product}</Anchor><Text size="sm">{active?.label ?? "页面不存在"}</Text></Breadcrumbs>
          <Group gap="sm" wrap="nowrap"><div role="status">{status}</div><ActionIcon aria-label="切换主题" variant="default" onClick={() => toggleColorScheme()}><Icon path={colorScheme === "dark" ? "M12 3v2 M12 19v2 M3 12h2 M19 12h2 M5.6 5.6 7 7 M17 17l1.4 1.4 M5.6 18.4 7 17 M17 7l1.4-1.4 M16 12a4 4 0 1 1-8 0 4 4 0 0 1 8 0" : "M20.9 13A9 9 0 0 1 11 3.1 9 9 0 1 0 20.9 13"} /></ActionIcon>{headerActions}</Group>
        </Group>
      </AppShell.Header>
      <AppShell.Navbar p="md" id="main-navigation"><AppShell.Section grow>{sidebarTitle && <Text size="xs" c="dimmed" mb="sm">{sidebarTitle}</Text>}<nav aria-label="主要导航">{items.map(item => <NavLink href={`#${item.path}`} key={item.path} label={item.label} leftSection={item.icon} active={activePath === item.path} aria-current={activePath === item.path ? "page" : undefined} onClick={close} />)}</nav></AppShell.Section>{sidebarFooter && <AppShell.Section><Divider my="md" />{sidebarFooter}</AppShell.Section>}</AppShell.Navbar>
      <AppShell.Main id="main-content" tabIndex={-1}><Container size={contentWidth} px={0}>{children}</Container></AppShell.Main>
    </AppShell>
  </>;
}
