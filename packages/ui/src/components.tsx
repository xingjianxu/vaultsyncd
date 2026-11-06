import { useId, type ButtonHTMLAttributes, type ReactNode } from "react";

export function Button({ children, className = "", type = "button", ...props }: ButtonHTMLAttributes<HTMLButtonElement>) {
  return <button className={`vs-button ${className}`} type={type} {...props}>{children}</button>;
}

export interface TableColumn<T> { readonly key: string; readonly title: string; readonly render: (row: T) => ReactNode }
export function Table<T>({ caption, columns, rows, rowKey, emptyMessage }: {
  caption: string; columns: readonly TableColumn<T>[]; rows: readonly T[];
  rowKey: (row: T) => string; emptyMessage: string;
}) {
  return <div className="vs-table-wrap"><table><caption>{caption}</caption>
    <thead><tr>{columns.map(column => <th scope="col" key={column.key}>{column.title}</th>)}</tr></thead>
    <tbody>{rows.length === 0
      ? <tr><td colSpan={columns.length}>{emptyMessage}</td></tr>
      : rows.map(row => <tr key={rowKey(row)}>{columns.map(column => <td key={column.key}>{column.render(row)}</td>)}</tr>)}
    </tbody></table></div>;
}

/** 原生模态提供焦点围栏和 Escape；调用者控制 open/close 和业务动作。 */
export function Dialog({ title, children, onClose }: { title: string; children: ReactNode; onClose: () => void }) {
  const titleId = useId();
  return <dialog aria-labelledby={titleId} ref={element => {
    if (element && !element.open) element.showModal();
  }} onCancel={event => { event.preventDefault(); onClose(); }}>
    <h2 id={titleId}>{title}</h2>{children}<Button onClick={onClose}>关闭</Button>
  </dialog>;
}

export interface NavItem { readonly path: string; readonly label: string }
export function PageShell({ product, items, activePath, status, children }: {
  product: string; items: readonly NavItem[]; activePath: string; status: ReactNode; children: ReactNode;
}) {
  return <div className="vs-shell">
    <a className="vs-skip" href="#main-content" onClick={event => {
      event.preventDefault();
      event.currentTarget.closest(".vs-shell")?.querySelector<HTMLElement>("main")?.focus();
    }}>跳到主要内容</a>
    <aside><div className="vs-brand">vaultsyncd<span>{product}</span></div>
      <nav aria-label="主要导航">{items.map(item => <a href={`#${item.path}`} key={item.path}
        aria-current={activePath === item.path ? "page" : undefined}>{item.label}</a>)}</nav>
      <div className="vs-sidebar-note">文件有据可查，修改始终保留。</div>
    </aside>
    <div className="vs-workspace"><header><span>{product}</span><div role="status">{status}</div></header>
      <main id="main-content" tabIndex={-1}>{children}</main><footer>vaultsyncd · 文件同步与恢复</footer>
    </div>
  </div>;
}

export function EmptyState({ title, children }: { title: string; children: ReactNode }) {
  return <section className="vs-empty"><h2>{title}</h2><p>{children}</p></section>;
}
