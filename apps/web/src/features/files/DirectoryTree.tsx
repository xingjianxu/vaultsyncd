import { useEffect, useMemo, useState } from "react";
import { Tree, UnstyledButton, useTree, type TreeNodeData } from "@mantine/core";
import type { SnapshotEntry } from "@vaultsyncd/api-client";
import { ancestry, type DirectorySnapshot } from "./browser";
import { FileIcon } from "./FileIcon";
import classes from "./DirectoryTree.module.css";

export function DirectoryTree({ snapshot, directory, rootName, navigate }: {
  snapshot: DirectorySnapshot;
  directory?: SnapshotEntry;
  rootName: string;
  navigate: (entry: SnapshotEntry) => void;
}) {
  const data = useMemo(() => {
    const nodes = new Map<string, TreeNodeData>();
    for (const entry of snapshot.entries.values()) {
      if (entry.kind === "directory") nodes.set(entry.entry_id, { value: entry.entry_id, label: entry.name || rootName });
    }
    for (const [id, node] of nodes) {
      const children = (snapshot.children.get(id) ?? []).filter(entry => entry.kind === "directory");
      if (children.length) node.children = children.map(entry => nodes.get(entry.entry_id)!);
    }
    return [nodes.get(snapshot.root.entry_id)!];
  }, [snapshot, rootName]);
  const [expandedState, setExpandedState] = useState<Record<string, boolean>>({});
  const tree = useTree({
    expandedState,
    onExpandedStateChange: setExpandedState,
    selectedState: directory ? [directory.entry_id] : [],
    onSelectedStateChange: values => {
      if (values.length !== 1) return;
      const entry = snapshot.entries.get(values[0]);
      if (entry && entry.entry_id !== directory?.entry_id) navigate(entry);
    },
  });
  useEffect(() => {
    const path = directory ? ancestry(snapshot, directory) : [snapshot.root];
    setExpandedState(previous => ({ ...previous, ...Object.fromEntries(path.map(entry => [entry.entry_id, true])) }));
  }, [snapshot, directory?.entry_id]);

  return <Tree aria-label="目录树" data={data} tree={tree} classNames={classes} withLines levelOffset={20}
    selectOnClick expandOnClick={false} allowRangeSelection={false}
    onKeyDown={event => {
      if (event.key !== "Enter" || !(event.target as HTMLElement).matches('[role="treeitem"]')) return;
      const id = (event.target as HTMLElement).dataset.value;
      const entry = id ? snapshot.entries.get(id) : undefined;
      if (entry) { event.preventDefault(); navigate(entry); }
    }}
    renderNode={({ node, expanded, hasChildren, elementProps }) => <div {...elementProps} title={String(node.label)}>
      {hasChildren ? <UnstyledButton className={classes.folder} tabIndex={-1}
        aria-label={`${expanded ? "收起" : "展开"} ${node.label}`}
        onClick={event => { event.stopPropagation(); tree.toggleExpanded(node.value); }}>
        <FileIcon name={expanded ? "folder-open" : "folder"} />
      </UnstyledButton> : <span className={classes.folder}><FileIcon name="folder" /></span>}
      <span>{node.label}</span>
    </div>} />;
}
