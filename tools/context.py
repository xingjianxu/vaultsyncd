#!/usr/bin/env python3
"""按标题精确提取实施手册任务卡与 design 小节，供会话按预算取用。

仅使用 Python 3 标准库。接口契约见 docs/architecture/interfaces.md。
"""

from __future__ import annotations

import argparse
import difflib
import re
import sys
from dataclasses import dataclass
from pathlib import Path

REPO_VAULT = Path(__file__).resolve().parent.parent
CARD_FILE = REPO_VAULT / "agent" / "implementation-plan.md"
DESIGN_FILE = REPO_VAULT / "docs" / "design.md"

ATX_HEADING = re.compile(r"^(#{1,6})[ \t]+(.+?)[ \t]*$")
FENCE_OPEN = re.compile(r"^ {0,3}(`{3,}|~{3,})(.*)$")
TITLE_SUFFIX_HASHES = re.compile(r"[ \t]+#+$")
CARD_SEPARATOR = re.compile(r"\s+[—–]\s+")
DESIGN_NUMBER = re.compile(r"^\d+(?:\.\d+)*$")
CARD_ID = re.compile(r"^S\d+$", re.IGNORECASE)
EN_DASH = "\u2013"

MAX_LISTED_CANDIDATES = 20
MAX_SUGGESTIONS = 8


class QueryError(Exception):
    """查询无法唯一解析时抛出；消息直接展示给调用者。"""


@dataclass(frozen=True)
class Heading:
    index: int  # 0 基行号
    level: int
    title: str
    raw: str  # 原始标题行（含 # 前缀）
    key: str | None  # 任务卡编号 S002 或 design 编号 11.2
    aliases: tuple[str, ...]  # 归一化后的可查询文本


@dataclass(frozen=True)
class Document:
    display: str
    lines: list[str]
    headings: list[Heading]
    unclosed_fence_line: int | None


@dataclass(frozen=True)
class Section:
    document: Document
    heading: Heading
    start: int  # 0 基，含标题行
    end: int  # 0 基，含范围内最后一个非空行


def normalize(text: str) -> str:
    text = text.replace(EN_DASH, "—")
    text = re.sub(r"\s+", " ", text.strip().lower())
    return text


def heading_aliases(mode: str, title: str) -> tuple[str, tuple[str, ...]]:
    """返回 (编号键, 可查询文本集合)。无法编号的标题键为 None。"""
    parts = title.split(maxsplit=1)
    if not parts:
        return None, ()
    first_token = parts[0]
    aliases: list[str] = []
    if mode == "design":
        key = first_token.rstrip(".")
        if not DESIGN_NUMBER.match(key):
            return None, (normalize(title),)
        aliases.append(normalize(key))
        if len(parts) > 1:
            aliases.append(normalize(parts[1]))
        aliases.append(normalize(title))
        return key, tuple(aliases)
    key = first_token if CARD_ID.match(first_token) else None
    if key is not None:
        aliases.append(normalize(key))
        body = CARD_SEPARATOR.split(title, maxsplit=1)
        if len(body) == 2:
            aliases.append(normalize(body[1]))
        aliases.append(normalize(title))
        return key, tuple(aliases)
    return None, (normalize(title),)


def scan_headings(lines: list[str], mode: str) -> tuple[list[Heading], int | None]:
    """识别标题；代码围栏内的 # 行不算标题。返回 (标题列表, 未闭合围栏行号)。"""
    headings: list[Heading] = []
    fence: tuple[str, int] | None = None
    fence_start: int | None = None
    for index, line in enumerate(lines):
        body = line.lstrip(" ")
        indent = len(line) - len(body)
        if fence is not None:
            character, length = fence
            if indent <= 3:
                run = len(body) - len(body.lstrip(character))
                if run >= length and not body[run:].strip():
                    fence = None
                    fence_start = None
            continue
        fence_match = FENCE_OPEN.match(line)
        if fence_match:
            marker, info = fence_match.group(1), fence_match.group(2)
            # 反引号围栏的 info 字符串含反引号时不构成围栏（CommonMark 规则）。
            if not (marker[0] == "`" and "`" in info):
                fence = (marker[0], len(marker))
                fence_start = index
                continue
        heading_match = ATX_HEADING.match(line)
        if not heading_match:
            continue
        title = TITLE_SUFFIX_HASHES.sub("", heading_match.group(2)).strip()
        if not title:
            continue
        key, aliases = heading_aliases(mode, title)
        headings.append(
            Heading(
                index=index,
                level=len(heading_match.group(1)),
                title=title,
                raw=line,
                key=key,
                aliases=aliases,
            )
        )
    return headings, fence_start


def load_document(mode: str) -> Document:
    path = CARD_FILE if mode == "card" else DESIGN_FILE
    display = path.relative_to(REPO_VAULT).as_posix()
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except OSError as error:
        raise SystemExit(f"context.py: error: 无法读取 {display}: {error}")
    headings, unclosed = scan_headings(lines, mode)
    return Document(display=display, lines=lines, headings=headings, unclosed_fence_line=unclosed)


def candidate_label(document: Document, heading: Heading) -> str:
    return f"{document.display}:{heading.index + 1}  {heading.title}"


def list_candidates(document: Document, headings: list[Heading]) -> str:
    shown = headings[:MAX_LISTED_CANDIDATES]
    lines = [f"  {candidate_label(document, heading)}" for heading in shown]
    if len(headings) > len(shown):
        lines.append(f"  …另有 {len(headings) - len(shown)} 个候选未列出")
    return "\n".join(lines)


def suggestions(document: Document, mode: str, query: str) -> list[Heading]:
    addressable = [heading for heading in document.headings if mode == "design" or heading.key is not None]
    pool = [(heading, alias) for heading in addressable for alias in heading.aliases]
    close_aliases = difflib.get_close_matches(query, [alias for _, alias in pool], n=MAX_SUGGESTIONS, cutoff=0.5)
    found: list[Heading] = []
    for alias in close_aliases:
        for heading, heading_alias in pool:
            if heading_alias == alias and heading not in found:
                found.append(heading)
                break
    structural: list[Heading] = []
    if mode == "design" and DESIGN_NUMBER.match(query):
        parent = query.rsplit(".", 1)[0] if "." in query else query
        structural = [
            heading
            for heading in addressable
            if heading.key and (heading.key == parent or heading.key.startswith(parent + "."))
        ]
    elif mode == "card" and len(query) >= 2:
        prefix = query[:2]
        structural = [heading for heading in addressable if heading.key and heading.key.lower().startswith(prefix)]
    return (found + [heading for heading in structural if heading not in found])[:MAX_SUGGESTIONS]


def resolve_one(document: Document, mode: str, raw_query: str) -> Heading:
    query = normalize(raw_query)
    if mode == "design":
        query = query.lstrip("§").strip()
        if query.endswith("."):
            query = query[:-1]
    if not query:
        raise QueryError("空查询：请给出任务卡编号/标题或 design 小节编号。")
    # card 模式只把带 S 编号的标题当作可直接定位的任务卡；design 允许非编号标题（如附录）按标题定位。
    candidates = [heading for heading in document.headings if mode == "design" or heading.key is not None]
    exact = [heading for heading in candidates if query in heading.aliases]
    if len(exact) == 1:
        return exact[0]
    if len(exact) > 1:
        raise QueryError(
            f"查询 {raw_query!r} 命中 {len(exact)} 个相同标题，无法确定唯一结果：\n"
            + list_candidates(document, exact)
        )
    key_prefix: list[Heading] = []
    if mode == "design":
        key_prefix = [
            heading
            for heading in candidates
            if heading.key and heading.key.startswith(query + ".")
        ]
    alias_prefix = [heading for heading in candidates if any(alias.startswith(query) for alias in heading.aliases)]
    prefix = key_prefix + [heading for heading in alias_prefix if heading not in key_prefix]
    if len(prefix) == 1:
        return prefix[0]
    if len(prefix) > 1:
        raise QueryError(
            f"查询 {raw_query!r} 是前缀且命中 {len(prefix)} 个标题，请给出完整编号或标题：\n"
            + list_candidates(document, prefix)
        )
    message = f"未找到匹配 {raw_query!r} 的标题（{document.display}）。"
    nearby = suggestions(document, mode, query)
    if nearby:
        message += "\n接近的候选：\n" + list_candidates(document, nearby)
    raise QueryError(message)


def section_end(document: Document, heading: Heading, mode: str) -> int:
    """返回标题下一行起算的结束行（0 基，不含），到同级/更高级或非后代编号标题为止。"""
    if mode == "design" and heading.key and DESIGN_NUMBER.match(heading.key):
        target = tuple(int(part) for part in heading.key.split("."))
        for other in document.headings:
            if other.index <= heading.index:
                continue
            if other.key and DESIGN_NUMBER.match(other.key):
                number = tuple(int(part) for part in other.key.split("."))
                if number[: len(target)] == target:
                    continue
                return other.index
            if other.level <= heading.level:
                return other.index
        return len(document.lines)
    for other in document.headings:
        if other.index > heading.index and other.level <= heading.level:
            return other.index
    return len(document.lines)


def build_section(document: Document, heading: Heading, mode: str) -> Section:
    limit = section_end(document, heading, mode)
    end = heading.index
    for index in range(limit - 1, heading.index - 1, -1):
        if document.lines[index].strip():
            end = index
            break
    return Section(document=document, heading=heading, start=heading.index, end=end)


def format_section(section: Section) -> str:
    lines = [
        f"--- source: {section.document.display}",
        f"--- heading: {section.heading.raw.strip()}",
        f"--- lines: {section.start + 1}-{section.end + 1}",
        "",
    ]
    lines.extend(section.document.lines[section.start : section.end + 1])
    return "\n".join(lines)


def warn_unclosed_fence(document: Document, sections: list[Section]) -> None:
    """未闭合围栏会隐藏其后标题；仅当本次输出可能受其影响时提示。"""
    fence_line = document.unclosed_fence_line
    if fence_line is None:
        return
    if not any(section.end >= fence_line or section.start > fence_line for section in sections):
        return
    print(
        f"context.py: 注意：{document.display} 第 {fence_line + 1} 行的代码围栏未闭合，"
        "其后各行不参与标题识别。",
        file=sys.stderr,
    )


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="python3 tools/context.py",
        description="按标题提取实施手册任务卡或 docs/design.md 小节，输出带来源与行号范围的原文。",
        epilog=(
            "示例：\n"
            "  python3 tools/context.py card S002\n"
            "  python3 tools/context.py card 'S002 — 建立 agent 入口与上下文提取工具'\n"
            "  python3 tools/context.py design 11.2\n"
            "  python3 tools/context.py design 5.2 5.9 5.11\n"
            "  一次多个参数按顺序输出；任一参数无法解析时不输出任何小节并以 2 退出。\n"
            "退出码：0 成功；2 用法或查询错误；1 源文件读取失败。"
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    subparsers = parser.add_subparsers(dest="command", required=True)
    card_parser = subparsers.add_parser(
        "card",
        help="提取 agent/implementation-plan.md 的任务卡",
        description="按任务卡编号（S002）或完整标题（含中文标题）提取单张卡，到下一张卡为止。",
    )
    card_parser.add_argument("queries", nargs="+", metavar="CARD", help="任务卡编号或标题，可给出多个")
    design_parser = subparsers.add_parser(
        "design",
        help="提取 docs/design.md 的小节",
        description="按编号（11.2）或标题提取 design 小节，含其下属子标题，到同级/更高级或非后代编号标题为止。",
    )
    design_parser.add_argument("queries", nargs="+", metavar="SECTION", help="小节编号或标题，可给出多个")
    return parser


def main(argv: list[str] | None = None) -> int:
    parser = build_parser()
    arguments = parser.parse_args(argv)
    mode = arguments.command
    document = load_document(mode)
    sections: list[Section] = []
    errors: list[str] = []
    seen: set[tuple[str, int]] = set()
    for query in arguments.queries:
        try:
            heading = resolve_one(document, mode, query)
        except QueryError as error:
            errors.append(str(error))
            continue
        identity = (document.display, heading.index)
        if identity in seen:
            continue
        seen.add(identity)
        sections.append(build_section(document, heading, mode))
    if errors:
        for error in errors:
            print(f"context.py: error: {error}", file=sys.stderr)
        return 2
    warn_unclosed_fence(document, sections)
    print("\n\n".join(format_section(section) for section in sections))
    return 0


if __name__ == "__main__":
    sys.exit(main())
