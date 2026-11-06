import { useEffect, useState } from "react";

// Hash 路由可直接刷新，也适合后续桌面静态资源协议；未知路径显式显示不存在。
export function useRoute(defaultPath: string) {
  const read = () => window.location.hash.slice(1) || defaultPath;
  const [path, setPath] = useState(() => typeof window === "undefined" ? defaultPath : read());
  useEffect(() => {
    const update = () => setPath(read());
    window.addEventListener("hashchange", update);
    return () => window.removeEventListener("hashchange", update);
  }, [defaultPath]);
  return path;
}
