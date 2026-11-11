import type { ReactNode } from "react";
import { ActionIcon, Anchor, AppShell, Breadcrumbs, Burger, Container, Divider, Group, NavLink, Text, useComputedColorScheme, useMantineColorScheme } from "@mantine/core";
import { useDisclosure } from "@mantine/hooks";
import { Moon, Sun } from "lucide-react";
import { Icon } from "./Icons";

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
  brandIcon?: ReactNode;
}

/** 无路由或传输依赖：应用提供导航项、当前路径与真实连接状态。 */
export function PageShell({ product, items, activePath, status, children, homePath = items[0]?.path ?? "/", headerActions, sidebarTitle, sidebarFooter, contentWidth = "xl", brandIcon }: PageShellProps) {
  const [opened, { toggle, close }] = useDisclosure();
  const { toggleColorScheme } = useMantineColorScheme();
  const colorScheme = useComputedColorScheme("light");
  const active = items.find(item => item.path === activePath);
  return <>
    <a className="skip-link" href="#main-content" onClick={event => { event.preventDefault(); document.getElementById("main-content")?.focus(); }}>跳到主要内容</a>
    <AppShell header={{ height: 48 }} navbar={{ width: 240, breakpoint: "sm", collapsed: { mobile: !opened } }} padding="md">
      <AppShell.Header className="app-header">
        <Group className="app-brand" h="100%" px="md" gap="sm" wrap="nowrap"><Burger opened={opened} onClick={toggle} hiddenFrom="sm" size="sm" aria-label="切换导航" aria-controls="main-navigation" /><Anchor href={`#${homePath}`} fw={700} c="inherit" underline="never"><Group gap="xs" wrap="nowrap">{brandIcon}vaultsyncd</Group></Anchor></Group>
        <Group className="app-topbar" h="100%" px="md" gap="sm" wrap="nowrap">
          <Breadcrumbs visibleFrom="sm" aria-label="页面位置"><Anchor href={`#${homePath}`} size="sm">{product}</Anchor><Text size="sm">{active?.label ?? "页面不存在"}</Text></Breadcrumbs>
          <Group gap="sm" wrap="nowrap"><div role="status">{status}</div><ActionIcon aria-label="切换主题" variant="default" onClick={() => toggleColorScheme()}><Icon icon={colorScheme === "dark" ? Sun : Moon} /></ActionIcon>{headerActions}</Group>
        </Group>
      </AppShell.Header>
      <AppShell.Navbar p="md" id="main-navigation"><AppShell.Section grow>{sidebarTitle && <Text size="xs" c="dimmed" mb="sm">{sidebarTitle}</Text>}<nav aria-label="主要导航">{items.map(item => <NavLink href={`#${item.path}`} key={item.path} label={item.label} leftSection={item.icon} active={activePath === item.path} aria-current={activePath === item.path ? "page" : undefined} onClick={close} />)}</nav></AppShell.Section>{sidebarFooter && <AppShell.Section><Divider my="md" />{sidebarFooter}</AppShell.Section>}</AppShell.Navbar>
      <AppShell.Main id="main-content" tabIndex={-1}><Container size={contentWidth} px={0}>{children}</Container></AppShell.Main>
    </AppShell>
  </>;
}
