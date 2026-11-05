//! 把校验后的契约模型渲染成 Rust DTO 与 TypeScript 客户端。
//!
//! 渲染只依赖 [`Model`] 与 [`GenerationConfig`] 的映射表，因此同一契约必然得到同一份字节：
//! 没有时间戳、没有随机顺序（schema 与字段都按名字排序），`cargo xtask generate --check`
//! 才能用逐字节比对来守护“生成代码与契约同步”。
//!
//! 生成的 Rust 必须同时通过 `rustfmt --check`，生成代码的格式因此在这里就按 rustfmt 的默认风格
//! 产出；排版改动会在重新生成时体现为 diff，字面量不会在运行时被重新格式化。

use super::{
    ApiOperation, DO_NOT_EDIT, FieldType, GenerationConfig, Model, SchemaDef, TargetTypes,
};

/// 渲染全部输出，顺序与 `api/generation.toml` 的 `[outputs]` 一致。
pub(super) fn render(config: &GenerationConfig, model: &Model) -> Vec<(String, String)> {
    vec![
        (config.outputs.rust.clone(), render_rust(config, model)),
        (
            config.outputs.typescript_types.clone(),
            render_typescript_types(config, model),
        ),
        (
            config.outputs.typescript_client.clone(),
            render_typescript_client(model),
        ),
    ]
}

/// 生成文件表头的公共信息。
fn provenance(model: &Model) -> String {
    format!(
        "契约：{}；生成器：{} {}",
        model.source_line(),
        super::GENERATOR,
        env!("CARGO_PKG_VERSION")
    )
}

/// 追加契约说明的原文行（按源文件断行，不重新折行，避免中文换行处出现多余空格）。
fn push_description(out: &mut String, prefix: &str, model: &Model) {
    for line in &model.description {
        out.push_str(&format!(
            "{prefix} {line}
"
        ));
    }
}

/// 渲染 Rust DTO 文件。
fn render_rust(config: &GenerationConfig, model: &Model) -> String {
    let types = &config.types.rust;
    let mut out = String::new();
    out.push_str(&format!("//! {DO_NOT_EDIT}\n//!\n"));
    push_wrapped(&mut out, "//!", &provenance(model));
    push_description(&mut out, "//!", model);
    out.push_str("//!\n");
    out.push_str(
        "//! 生成代码只做 wire 形态：闭集枚举的未知取值在反序列化时报错，十进制长整数只接受\n",
    );
    out.push_str(
        "//! 规范文本（见 `crate::decimal`）；domain 类型到 wire 的转换在 `crate::domain_map`。\n",
    );
    out.push('\n');
    if model_uses_map(model) {
        out.push_str("use std::collections::BTreeMap;\n\n");
    }

    for (name, definition) in &model.schemas {
        match definition {
            SchemaDef::Struct { doc, fields } => {
                push_rust_doc(&mut out, "", doc);
                out.push_str(
                    "#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize)]\n",
                );
                out.push_str(&format!("pub struct {name} {{\n"));
                for field in fields {
                    push_rust_doc(&mut out, "    ", &field.doc);
                    if matches!(field.ty, FieldType::Decimal) {
                        out.push_str("    #[serde(with = \"crate::decimal\")]\n");
                    }
                    let mut ty = rust_type(types, &field.ty);
                    if field.optional {
                        ty = format!("Option<{ty}>");
                    }
                    out.push_str(&format!("    pub {}: {ty},\n", field.wire));
                }
                out.push_str("}\n\n");
            }
            SchemaDef::Enum { doc, variants } => {
                push_rust_doc(&mut out, "", doc);
                out.push_str(
                    "#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]\n",
                );
                out.push_str(&format!("pub enum {name} {{\n"));
                for variant in variants {
                    out.push_str(&format!("    /// 取值 `{}`。\n", variant.value));
                    out.push_str(&format!("    #[serde(rename = \"{}\")]\n", variant.value));
                    out.push_str(&format!("    {},\n", variant.name));
                }
                out.push_str("}\n\n");
            }
        }
    }
    trim_trailing_blank_line(&mut out);
    out
}

/// 渲染 TypeScript 类型与解码器文件。
fn render_typescript_types(config: &GenerationConfig, model: &Model) -> String {
    let types = &config.types.typescript;
    let mut out = String::new();
    out.push_str(&format!("// {DO_NOT_EDIT}\n//\n"));
    push_wrapped(&mut out, "//", &provenance(model));
    push_description(&mut out, "//", model);
    out.push_str("//\n");
    out.push_str("// 解码器只做 wire 形态校验：闭集枚举的未知取值、缺失的必填字段与非法十进制文本都会抛错，\n");
    out.push_str("// 调用方不会拿到“看起来可用”的默认值。\n\n");

    for helper in used_helpers(model) {
        out.push_str(helper_source(helper));
    }

    for (name, definition) in &model.schemas {
        match definition {
            SchemaDef::Struct { doc, fields } => {
                push_ts_doc(&mut out, "", doc);
                out.push_str(&format!("export interface {name} {{\n"));
                for field in fields {
                    push_ts_doc(&mut out, "  ", &field.doc);
                    let ty = ts_type(types, &field.ty);
                    let marker = if field.optional { "?" } else { "" };
                    out.push_str(&format!("  {}{marker}: {ty};\n", field.wire));
                }
                out.push_str("}\n\n");
                out.push_str(&format!(
                    "export function decode{name}(value: unknown): {name} {{\n  const object = expect_object(value, \"{name}\");\n  return {{\n"
                ));
                for field in fields {
                    let what = format!("{name}.{}", field.wire);
                    let expr = ts_decode(&field.ty, &format!("object.{}", field.wire), &what);
                    let expr = if field.optional {
                        format!("object.{} === undefined ? undefined : {expr}", field.wire)
                    } else {
                        expr
                    };
                    out.push_str(&format!("    {}: {expr},\n", field.wire));
                }
                out.push_str("  };\n}\n\n");
            }
            SchemaDef::Enum { doc, variants } => {
                push_ts_doc(&mut out, "", doc);
                let values: Vec<String> = variants
                    .iter()
                    .map(|variant| format!("\"{}\"", variant.value))
                    .collect();
                out.push_str(&format!("export type {name} = {};\n\n", values.join(" | ")));
                let constant = screaming_snake(name);
                out.push_str(&format!(
                    "export const {constant}_VALUES = [{}] as const;\n\n",
                    values.join(", ")
                ));
                out.push_str(&format!(
                    "export function decode{name}(value: unknown): {name} {{\n  if (typeof value !== \"string\" || !({constant}_VALUES as readonly string[]).includes(value)) {{\n    throw new Error(`未知的 {name} 取值：${{JSON.stringify(value)}}`);\n  }}\n  return value as {name};\n}}\n\n"
                ));
            }
        }
    }
    trim_trailing_blank_line(&mut out);
    out
}

/// 渲染 TypeScript 端点调用文件。
fn render_typescript_client(model: &Model) -> String {
    let mut out = String::new();
    out.push_str(&format!("// {DO_NOT_EDIT}\n//\n"));
    push_wrapped(&mut out, "//", &provenance(model));
    push_description(&mut out, "//", model);
    out.push_str("//\n");
    out.push_str(
        "// 传输细节（fetch、request_id）由调用方注入，客户端不直接读全局对象；非 2xx 响应按错误\n",
    );
    out.push_str("// 信封解码成 ApiError，未知错误码与畸形响应抛 ProtocolError。\n\n");

    let mut decoders = vec![format!("decode{}", super::ERROR_SCHEMA)];
    let mut types = vec![error_body_type(model)];
    for operation in &model.operations {
        decoders.push(format!("decode{}", operation.success_type));
        types.push(operation.success_type.clone());
    }
    decoders.sort();
    decoders.dedup();
    types.sort();
    types.dedup();
    out.push_str(&format!(
        "import {{ {} }} from \"./types\";\n",
        decoders.join(", ")
    ));
    out.push_str(&format!(
        "import type {{ {} }} from \"./types\";\n\n",
        types.join(", ")
    ));

    out.push_str(CLIENT_RUNTIME);
    for operation in &model.operations {
        push_client_operation(&mut out, operation);
    }
    trim_trailing_blank_line(&mut out);
    out
}

/// 错误信封里主体类型的名字；用于 `ApiError` 的 `body` 字段。
fn error_body_type(model: &Model) -> String {
    let SchemaDef::Struct { fields, .. } = &model.schemas[super::ERROR_SCHEMA] else {
        unreachable!("{} 必须是 object schema", super::ERROR_SCHEMA)
    };
    let body = fields
        .iter()
        .find(|field| field.wire == "error")
        .expect("错误信封必须有 error 字段");
    match &body.ty {
        FieldType::Named(name) => name.clone(),
        other => unreachable!("错误信封的 error 字段必须是具名 schema，实际 {other:?}"),
    }
}

/// 端点之外的共享运行时：选项、错误类型与小工具。
const CLIENT_RUNTIME: &str = "\
/** 注入的 fetch 形状：浏览器/Node 的全局 fetch 或测试替身。 */
export type FetchLike = (url: string, init?: RequestInit) => Promise<Response>;

/** 一次请求需要的传输参数。 */
export interface ClientOptions {
  /** 服务端根地址，例如 https://vault.example:8443（结尾斜杠可有可无）。 */
  base_url: string;
  /** fetch 实现。 */
  fetch: FetchLike;
  /** 生成 x-request-id 头；每次调用必须给出新值，供服务端日志关联。 */
  request_id: () => string;
}

/** 服务端返回的标准错误信封（design §15.4）。 */
export class ApiError extends Error {
  /** HTTP 状态码。 */
  readonly status: number;
  /** 已解码的错误主体。 */
  readonly body: ErrorBody;

  constructor(status: number, body: ErrorBody) {
    super(`${body.code}: ${body.message}`);
    this.name = \"ApiError\";
    this.status = status;
    this.body = body;
  }
}

/** 响应不是契约允许的形状：未知枚举、字段缺失、非法十进制文本或非 JSON。 */
export class ProtocolError extends Error {
  constructor(message: string) {
    super(message);
    this.name = \"ProtocolError\";
  }
}

function join_url(base_url: string, path: string): string {
  return `${base_url.replace(/\\/$/, \"\")}${path}`;
}

function request_headers(options: ClientOptions, init?: RequestInit): Headers {
  const headers = new Headers(init?.headers);
  headers.set(\"accept\", \"application/json\");
  headers.set(\"x-request-id\", options.request_id());
  return headers;
}

async function read_json(response: Response): Promise<unknown> {
  try {
    return (await response.json()) as unknown;
  } catch {
    throw new ProtocolError(`响应不是 JSON（HTTP ${response.status}）`);
  }
}

function api_error(status: number, body: unknown): ApiError {
  return new ApiError(status, decodeErrorEnvelope(body).error);
}

";

/// 单个端点的调用函数。
fn push_client_operation(out: &mut String, operation: &ApiOperation) {
    out.push_str("/**\n");
    for line in &operation.doc {
        if line.is_empty() {
            out.push_str(" *\n");
        } else {
            out.push_str(&format!(" * {line}\n"));
        }
    }
    out.push_str(" */\n");
    out.push_str(&format!(
        "export async function {}(options: ClientOptions, init?: RequestInit): Promise<{}> {{\n",
        operation.operation_id, operation.success_type
    ));
    out.push_str(&format!(
        "  const response = await options.fetch(join_url(options.base_url, \"{}\"), {{\n    ...init,\n    method: \"{}\",\n    headers: request_headers(options, init),\n  }});\n",
        operation.path, operation.method
    ));
    out.push_str("  const body = await read_json(response);\n");
    out.push_str("  if (!response.ok) {\n    throw api_error(response.status, body);\n  }\n");
    out.push_str(&format!(
        "  return decode{}(body);\n}}\n\n",
        operation.success_type
    ));
}

/// Rust 目标类型。
fn rust_type(types: &TargetTypes, ty: &FieldType) -> String {
    match ty {
        FieldType::Boolean => types.boolean.clone(),
        FieldType::Integer => types.integer.clone(),
        FieldType::Text => types.string.clone(),
        FieldType::Decimal => types.decimal_u64.clone(),
        FieldType::Named(name) => name.clone(),
        FieldType::Array(inner) => types.array.replace("{item}", &rust_type(types, inner)),
        FieldType::Map(inner) => types.map.replace("{value}", &rust_type(types, inner)),
    }
}

/// TypeScript 目标类型。
fn ts_type(types: &TargetTypes, ty: &FieldType) -> String {
    match ty {
        FieldType::Boolean => types.boolean.clone(),
        FieldType::Integer => types.integer.clone(),
        FieldType::Text => types.string.clone(),
        FieldType::Decimal => types.decimal_u64.clone(),
        FieldType::Named(name) => name.clone(),
        FieldType::Array(inner) => types.array.replace("{item}", &ts_type(types, inner)),
        FieldType::Map(inner) => types.map.replace("{value}", &ts_type(types, inner)),
    }
}

/// TypeScript 解码表达式：每个 wire 类型都有对应的运行时校验。
fn ts_decode(ty: &FieldType, expr: &str, what: &str) -> String {
    match ty {
        FieldType::Boolean => format!("expect_boolean({expr}, \"{what}\")"),
        FieldType::Integer => format!("expect_number({expr}, \"{what}\")"),
        FieldType::Text => format!("expect_string({expr}, \"{what}\")"),
        FieldType::Decimal => format!("expect_decimal({expr}, \"{what}\")"),
        FieldType::Named(name) => format!("decode{name}({expr})"),
        FieldType::Array(inner) => {
            let item = ts_decode(inner, "item", &format!("{what} 的元素"));
            format!("expect_array({expr}, \"{what}\").map((item) => {item})")
        }
        FieldType::Map(_) => format!("expect_string_map({expr}, \"{what}\")"),
    }
}

/// 解码辅助函数名：按需生成，避免产出用不到的代码。
fn used_helpers(model: &Model) -> Vec<&'static str> {
    let mut used: Vec<&'static str> = Vec::new();
    for definition in model.schemas.values() {
        let SchemaDef::Struct { fields, .. } = definition else {
            continue;
        };
        push_unique(&mut used, "expect_object");
        for field in fields {
            mark_helpers(&field.ty, &mut used);
        }
    }
    used
}

/// 递归收集一个字段类型需要的辅助函数。
fn mark_helpers(ty: &FieldType, used: &mut Vec<&'static str>) {
    match ty {
        FieldType::Boolean => push_unique(used, "expect_boolean"),
        FieldType::Integer => push_unique(used, "expect_number"),
        FieldType::Text => push_unique(used, "expect_string"),
        FieldType::Decimal => push_unique(used, "expect_decimal"),
        FieldType::Array(inner) => {
            push_unique(used, "expect_array");
            mark_helpers(inner, used);
        }
        FieldType::Map(_) => push_unique(used, "expect_string_map"),
        FieldType::Named(_) => {}
    }
}

/// 按固定顺序登记辅助函数名，保证输出稳定。
fn push_unique(used: &mut Vec<&'static str>, name: &'static str) {
    if !used.contains(&name) {
        used.push(name);
    }
}

/// 单个解码辅助函数的源码；顺序固定，保证输出稳定。
fn helper_source(name: &str) -> &'static str {
    match name {
        "expect_object" => {
            "function expect_object(value: unknown, what: string): Record<string, unknown> {\n  if (typeof value !== \"object\" || value === null || Array.isArray(value)) {\n    throw new Error(`期望 ${what} 是对象`);\n  }\n  return value as Record<string, unknown>;\n}\n\n"
        }
        "expect_string" => {
            "function expect_string(value: unknown, what: string): string {\n  if (typeof value !== \"string\") {\n    throw new Error(`期望 ${what} 是字符串`);\n  }\n  return value;\n}\n\n"
        }
        "expect_number" => {
            "function expect_number(value: unknown, what: string): number {\n  if (typeof value !== \"number\" || !Number.isInteger(value)) {\n    throw new Error(`期望 ${what} 是整数`);\n  }\n  return value;\n}\n\n"
        }
        "expect_boolean" => {
            "function expect_boolean(value: unknown, what: string): boolean {\n  if (typeof value !== \"boolean\") {\n    throw new Error(`期望 ${what} 是布尔值`);\n  }\n  return value;\n}\n\n"
        }
        "expect_decimal" => {
            "function expect_decimal(value: unknown, what: string): string {\n  if (typeof value !== \"string\" || !/^(0|[1-9][0-9]*)$/.test(value)) {\n    throw new Error(`期望 ${what} 是十进制无符号整数文本`);\n  }\n  return value;\n}\n\n"
        }
        "expect_array" => {
            "function expect_array(value: unknown, what: string): unknown[] {\n  if (!Array.isArray(value)) {\n    throw new Error(`期望 ${what} 是数组`);\n  }\n  return value;\n}\n\n"
        }
        "expect_string_map" => {
            "function expect_string_map(value: unknown, what: string): Record<string, string> {\n  const object = expect_object(value, what);\n  for (const [key, item] of Object.entries(object)) {\n    if (typeof item !== \"string\") {\n      throw new Error(`期望 ${what}.${key} 是字符串`);\n    }\n  }\n  return object as Record<string, string>;\n}\n\n"
        }
        other => unreachable!("未定义解码辅助函数 {other}"),
    }
}

/// 是否有任何映射字段：决定 Rust 文件是否需要 `BTreeMap` 导入。
fn model_uses_map(model: &Model) -> bool {
    fn uses_map(ty: &FieldType) -> bool {
        match ty {
            FieldType::Map(_) => true,
            FieldType::Array(inner) => uses_map(inner),
            _ => false,
        }
    }
    model.schemas.values().any(|definition| match definition {
        SchemaDef::Struct { fields, .. } => fields.iter().any(|field| uses_map(&field.ty)),
        SchemaDef::Enum { .. } => false,
    })
}

/// 追加 Rust 文档注释：每个输入行一行 `///`，`indent` 与所在项的缩进一致（rustfmt 要求）。
fn push_rust_doc(out: &mut String, indent: &str, doc: &str) {
    for line in doc.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            out.push_str(&format!("{indent}///\n"));
        } else {
            out.push_str(&format!("{indent}/// {line}\n"));
        }
    }
}

/// 追加带缩进的 TypeScript JSDoc 注释。
fn push_ts_doc(out: &mut String, indent: &str, doc: &str) {
    let lines: Vec<&str> = doc.lines().map(str::trim_end).collect();
    if lines.len() == 1 {
        out.push_str(&format!("{indent}/** {} */\n", lines[0]));
        return;
    }
    out.push_str(&format!("{indent}/**\n"));
    for line in lines {
        if line.is_empty() {
            out.push_str(&format!("{indent} *\n"));
        } else {
            out.push_str(&format!("{indent} * {line}\n"));
        }
    }
    out.push_str(&format!("{indent} */\n"));
}

/// 按 `prefix` 前缀折行追加表头说明，避免生成超长单行。
fn push_wrapped(out: &mut String, prefix: &str, text: &str) {
    for line in wrap(text, 96 - prefix.len()) {
        out.push_str(&format!("{prefix} {line}\n"));
    }
}

/// 按显示宽度粗略折行：ASCII 记 1、其他字符记 2，够用即可，不做排版级精确对齐。
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0;
    for ch in text.chars() {
        let ch_width = if ch.is_ascii() { 1 } else { 2 };
        if current_width + ch_width > width && !current.is_empty() {
            lines.push(std::mem::take(&mut current));
            current_width = 0;
        }
        current.push(ch);
        current_width += ch_width;
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// 名字转 SCREAMING_SNAKE_CASE，用于 TypeScript 的取值常量名。
fn screaming_snake(name: &str) -> String {
    let mut out = String::new();
    for (index, ch) in name.char_indices() {
        if ch.is_ascii_uppercase() && index > 0 {
            out.push('_');
        }
        out.extend(ch.to_uppercase());
    }
    out
}

/// 文件末尾统一保留一个换行，去掉渲染时累积的空行。
fn trim_trailing_blank_line(out: &mut String) {
    while out.ends_with("\n\n") {
        out.pop();
    }
}
