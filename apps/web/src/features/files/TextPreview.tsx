import { memo, useEffect, useMemo, useState } from "react";
import { Alert, SegmentedControl, Stack, useComputedColorScheme } from "@mantine/core";
import CodeMirror from "@uiw/react-codemirror";
import { EditorView } from "@codemirror/view";
import { LanguageDescription, type LanguageSupport } from "@codemirror/language";
import { languages } from "@codemirror/language-data";
import Markdown from "react-markdown";
import remarkGfm from "remark-gfm";
import "./TextPreview.css";

export const MARKDOWN_BUDGET_MESSAGE = "Markdown 内容过于复杂，请切换代码模式或下载后查看。";
export const MAX_MARKDOWN_NODES = 10000;
export const MAX_MARKDOWN_CODE_BLOCKS = 32;
export const MAX_MARKDOWN_DEPTH = 128;
type MarkdownNode = { type: string; value?: string; children?: MarkdownNode[] };

/** 在创建 React 节点和 CodeMirror 实例前限制 Markdown 展开数量。 */
export function remarkPreviewBudget() {
  return (tree: MarkdownNode) => {
    let nodes = 1, codeBlocks = 0;
    const pending = [{ node: tree, depth: 0 }];
    const reject = () => {
      tree.children = [{ type: "paragraph", children: [{ type: "text", value: MARKDOWN_BUDGET_MESSAGE }] }];
    };
    while (pending.length) {
      const { node, depth } = pending.pop()!;
      if (depth > MAX_MARKDOWN_DEPTH) { reject(); return; }
      if (node.type === "code" && ++codeBlocks > MAX_MARKDOWN_CODE_BLOCKS) { reject(); return; }
      for (const child of node.children ?? []) {
        if (++nodes > MAX_MARKDOWN_NODES) { reject(); return; }
        pending.push({ node: child, depth: depth + 1 });
      }
    }
  };
}

function ReadOnlyCode({ text, name, signal, language, embedded = false }: {
  text: string; name: string; signal: AbortSignal; language?: string; embedded?: boolean;
}) {
  const theme = useComputedColorScheme("light");
  const [support, setSupport] = useState<LanguageSupport | null>(null);
  const [error, setError] = useState("");
  useEffect(() => {
    if (signal.aborted) return;
    let current = true;
    const stop = () => { current = false; };
    signal.addEventListener("abort", stop, { once: true });
    setSupport(null); setError("");
    const description = language
      ? LanguageDescription.matchLanguageName(languages, language, true)
      : LanguageDescription.matchFilename(languages, name);
    if (description) void description.load().then(value => {
      if (current) setSupport(value);
    }).catch(() => {
      if (current) setError("语法高亮加载失败，正在显示纯文本。");
    });
    return () => { stop(); signal.removeEventListener("abort", stop); };
  }, [name, language, signal]);
  const extensions = useMemo(() => [
    EditorView.cspNonce.of(typeof document === "undefined" ? "" : document.querySelector<HTMLMetaElement>('meta[name="csp-nonce"]')?.content ?? ""),
    EditorView.contentAttributes.of({ "aria-label": `文件代码 · ${name}`, "aria-readonly": "true", tabindex: "0" }),
    ...(support ? [support] : []),
  ], [support, name]);
  return <>
    {error && <Alert color="red" role="alert">{error}</Alert>}
    <CodeMirror className={embedded ? "file-preview-code file-preview-code-block" : "file-preview-code"}
      value={text} theme={theme} readOnly editable={false} extensions={extensions}
      maxHeight={embedded ? "24rem" : "68vh"}
      basicSetup={{ lineNumbers: !embedded, foldGutter: false, highlightActiveLine: false,
        highlightActiveLineGutter: false, highlightSelectionMatches: false,
        closeBrackets: false, autocompletion: false }} />
  </>;
}

// 保持相同的 Markdown renderer/CodeMirror 实例，关闭时不重建已渲染正文。
export const TextPreview = memo(function TextPreview({ text, name, markdown, signal }: { text: string; name: string; markdown: boolean; signal: AbortSignal }) {
  const [mode, setMode] = useState("preview");
  return <Stack gap="sm">
    {markdown && <SegmentedControl aria-label="Markdown 显示模式" value={mode} onChange={setMode}
      data={[{ label: "预览", value: "preview" }, { label: "代码", value: "code" }]} />}
    {markdown && mode === "preview" ? <article className="file-preview-markdown" aria-label={`Markdown 预览 · ${name}`}>
      <Markdown remarkPlugins={[remarkGfm, remarkPreviewBudget]} skipHtml components={{
        // 同步内容不触发资源请求或导航；HTML 也不进入 DOM。
        a: ({ children }) => <span>{children}</span>,
        img: ({ alt }) => <span>{alt ?? ""}</span>,
        pre: ({ children }) => <div className="file-preview-markdown-pre">{children}</div>,
        code: ({ children, className }) => {
          const language = /(?:^|\s)language-([^\s]+)/.exec(className ?? "")?.[1];
          return language ? <ReadOnlyCode text={String(children).replace(/\n$/, "")} name={name} signal={signal} language={language} embedded />
            : <code className={className}>{children}</code>;
        },
      }}>{text}</Markdown>
    </article> : <ReadOnlyCode text={text} name={name} signal={signal} />}
  </Stack>;
});
