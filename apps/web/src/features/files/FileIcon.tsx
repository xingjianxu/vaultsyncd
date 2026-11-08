export function FileIcon({ name }: { name: "back" | "forward" | "up" | "refresh" | "properties" | "folder" | "folder-open" | "file" }) {
  const paths = {
    back: "M15 5l-7 7 7 7", forward: "M9 5l7 7-7 7", up: "M5 14l7-7 7 7 M12 7v14",
    refresh: "M20 7v5h-5 M20 12a8 8 0 1 0-2 6", properties: "M12 11v6 M12 7v.1 M21 12a9 9 0 1 1-18 0 9 9 0 0 1 18 0",
    folder: "M3 7h6l2 2h10v11H3z M3 7V4h6l2 3", file: "M6 3h8l4 4v14H6z M14 3v5h4",
    "folder-open": "M3 9V4h6l2 3h10v3 M3 9h6l2 2h11l-4 9H3z",
  };
  return <svg aria-hidden="true" className="files-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round"><path d={paths[name]} /></svg>;
}
