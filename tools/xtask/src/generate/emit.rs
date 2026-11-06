//! 把校验后的契约模型渲染成 Rust DTO 与 TypeScript 类型/客户端。
//!
//! 远端契约与本机 IPC 契约共用同一套 schema 渲染（对象/闭集枚举/基础类型/描述），差别只在文件
//! 表头与附加常量：远端多一个 HTTP 调用文件，本机 IPC 多一个协议版本与帧上限常量。两份契约的
//! 输出都只依赖模型与映射表，所以同一契约必然得到同一份字节：没有时间戳、没有随机顺序（schema
//! 与字段按名字排序），`cargo xtask generate --check` 才能用逐字节比对守住“生成代码与契约同步”。
//!
//! 生成的 Rust 必须同时通过 `rustfmt --check`，生成代码的格式因此在这里就按 rustfmt 的默认风格
//! 产出；排版改动会在重新生成时体现为 diff，字面量不会在运行时被重新格式化。

use std::collections::BTreeMap;

use super::local_ipc::{IpcProtocol, LocalIpcModel};
use super::{
    ApiOperation, FieldType, GenerationConfig, Model, RequestBodyKind, SchemaDef, SuccessBody,
    TargetTypes, do_not_edit,
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
            render_typescript_client(config, model),
        ),
    ]
}

/// 渲染本机 IPC 的两份输出：Rust DTO 与 TypeScript 类型。
pub(super) fn render_local_ipc(
    config: &GenerationConfig,
    model: &LocalIpcModel,
) -> Vec<(String, String)> {
    vec![
        (
            config.outputs.local_ipc_rust.clone(),
            render_local_ipc_rust(config, model),
        ),
        (
            config.outputs.local_ipc_typescript_types.clone(),
            render_local_ipc_types(config, model),
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
fn push_description(out: &mut String, prefix: &str, description: &[String]) {
    for line in description {
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
    out.push_str(&format!("//! {}\n//!\n", do_not_edit(&config.source)));
    push_wrapped(&mut out, "//!", &provenance(model));
    push_description(&mut out, "//!", &model.description);
    out.push_str("//!\n");
    out.push_str(
        "//! 生成代码只做 wire 形态：闭集枚举的未知取值在反序列化时报错，十进制长整数只接受\n",
    );
    out.push_str(
        "//! 规范文本（见 `crate::decimal`）；domain 类型到 wire 的转换在 `crate::domain_map`。\n",
    );
    out.push('\n');
    if schemas_use_map(&model.schemas) {
        out.push_str("use std::collections::BTreeMap;\n\n");
    }
    push_rust_schemas(&mut out, types, &model.schemas);
    trim_trailing_blank_line(&mut out);
    out
}

/// 渲染本机 IPC 的 Rust DTO 文件：同一套 schema 渲染，额外生成协议版本与帧上限常量。
fn render_local_ipc_rust(config: &GenerationConfig, model: &LocalIpcModel) -> String {
    let types = &config.types.rust;
    let mut out = String::new();
    out.push_str(&format!(
        "//! {}\n//!\n",
        do_not_edit(&config.local_ipc_source)
    ));
    push_wrapped(&mut out, "//!", &local_ipc_provenance(config, model));
    push_description(&mut out, "//!", &model.description);
    out.push_str("//!\n");
    out.push_str(
        "//! 生成代码只做 wire 形态：闭集枚举的未知取值在反序列化时报错，可选字段缺省时不出现在\n",
    );
    out.push_str(
        "//! JSON 里；帧的读写与命令处理在 `crate::framing`/`crate::service`，传输层（命名管道、\n",
    );
    out.push_str("//! socket）不在本 crate。\n\n");
    push_local_ipc_constants_rust(&mut out, model);
    if schemas_use_map(&model.schemas) {
        out.push_str("use std::collections::BTreeMap;\n\n");
    }
    push_rust_schemas(&mut out, types, &model.schemas);
    trim_trailing_blank_line(&mut out);
    out
}

/// 本机 IPC 生成文件的表头来源行。
fn local_ipc_provenance(config: &GenerationConfig, model: &LocalIpcModel) -> String {
    format!(
        "契约：{}（{}）；生成器：{} {}",
        model.title,
        config.local_ipc_source,
        super::GENERATOR,
        env!("CARGO_PKG_VERSION")
    )
}

/// 本机 IPC 的协议版本与帧上限常量（Rust）。
fn push_local_ipc_constants_rust(out: &mut String, model: &LocalIpcModel) {
    let IpcProtocol { major, minor } = model.protocol;
    out.push_str("/// agent 支持的 IPC 协议 major 版本（major 不同即不兼容）。\n");
    out.push_str(&format!("pub const PROTOCOL_MAJOR: u32 = {major};\n"));
    out.push_str("/// agent 支持的 IPC 协议 minor 版本（高于此值的调用方被拒绝）。\n");
    out.push_str(&format!("pub const PROTOCOL_MINOR: u32 = {minor};\n"));
    out.push_str("/// 帧长度头的字节数：大端无符号整数，表示后续 JSON 负载的字节数。\n");
    out.push_str(&format!(
        "pub const FRAME_HEADER_BYTES: usize = {};\n",
        model.frame.header_bytes
    ));
    out.push_str("/// 单帧负载上限（字节）：超过即在读帧阶段拒绝，不为坏帧扩容。\n");
    out.push_str(&format!(
        "pub const MAX_FRAME_BYTES: usize = {};\n\n",
        model.frame.max_frame_bytes
    ));
}

/// 把 schema 列表渲染成 Rust 结构体与枚举（两种契约共用）。
fn push_rust_schemas(out: &mut String, types: &TargetTypes, schemas: &BTreeMap<String, SchemaDef>) {
    for (name, definition) in schemas {
        match definition {
            SchemaDef::Struct {
                doc,
                fields,
                closed,
            } => {
                push_rust_doc(out, "", doc);
                let secret = fields.iter().any(|field| field.wire == "password");
                let debug = if secret { "" } else { "Debug, " };
                let default = if fields.iter().all(|field| field.optional) {
                    ", Default"
                } else {
                    ""
                };
                out.push_str(&format!("#[derive({debug}Clone, PartialEq, Eq, serde::Deserialize, serde::Serialize{default})]\n"));
                if *closed {
                    out.push_str("#[serde(deny_unknown_fields)]\n");
                }
                out.push_str(&format!("pub struct {name} {{\n"));
                for field in fields {
                    push_rust_doc(out, "    ", &field.doc);
                    if matches!(field.ty, FieldType::Decimal) {
                        // 可选字段用 `decimal::option`：`with` 只对整体字段生效，而可选的十进制字段
                        // 是 `Option<String>`，不能直接交给 `String` 的读写实现。
                        let module = if field.optional {
                            "crate::decimal::option"
                        } else {
                            "crate::decimal"
                        };
                        out.push_str(&format!("    #[serde(with = \"{module}\")]\n"));
                    }
                    let mut ty = rust_type(types, &field.ty);
                    if field.optional {
                        ty = format!("Option<{ty}>");
                        // 可选字段缺省时不出现在 JSON 里：`null` 是第三种状态，两侧解码器都不接受。
                        // `default` 让字段缺失解成 `None`：带 `with` 的自定义读写实现不会自动套用
                        // serde 对 `Option` 的缺省规则，不写它会让可选十进制字段变成必填。
                        out.push_str("    #[serde(default)]\n");
                        out.push_str("    #[serde(skip_serializing_if = \"Option::is_none\")]\n");
                    }
                    out.push_str(&format!("    pub {}: {ty},\n", field.wire));
                }
                out.push_str("}\n\n");
                if secret {
                    out.push_str(&format!("impl core::fmt::Debug for {name} {{\n    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {{\n        let mut value = f.debug_struct(\"{name}\");\n"));
                    for field in fields {
                        if field.wire == "password" {
                            out.push_str("        value.field(\"password\", &\"[REDACTED]\");\n");
                        } else {
                            out.push_str(&format!(
                                "        value.field(\"{}\", &self.{});\n",
                                field.wire, field.wire
                            ));
                        }
                    }
                    out.push_str("        value.finish()\n    }\n}\n\n");
                }
            }
            SchemaDef::Enum { doc, variants } => {
                push_rust_doc(out, "", doc);
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
}

/// 渲染 TypeScript 类型与解码器文件。
fn render_typescript_types(config: &GenerationConfig, model: &Model) -> String {
    let types = &config.types.typescript;
    let mut out = String::new();
    out.push_str(&format!("// {}\n//\n", do_not_edit(&config.source)));
    push_wrapped(&mut out, "//", &provenance(model));
    push_description(&mut out, "//", &model.description);
    out.push_str("//\n");
    out.push_str("// 解码器只做 wire 形态校验：闭集枚举的未知取值、缺失的必填字段与非法十进制文本都会抛错，\n");
    out.push_str("// 调用方不会拿到“看起来可用”的默认值。\n\n");

    push_ts_helpers(&mut out, &model.schemas);
    push_ts_schemas(&mut out, types, &model.schemas);
    trim_trailing_blank_line(&mut out);
    out
}

/// 渲染本机 IPC 的 TypeScript 类型文件：同一套解码器，额外导出协议版本与帧上限常量。
fn render_local_ipc_types(config: &GenerationConfig, model: &LocalIpcModel) -> String {
    let types = &config.types.typescript;
    let mut out = String::new();
    out.push_str(&format!(
        "// {}\n//\n",
        do_not_edit(&config.local_ipc_source)
    ));
    push_wrapped(&mut out, "//", &local_ipc_provenance(config, model));
    push_description(&mut out, "//", &model.description);
    out.push_str("//\n");
    out.push_str("// 解码器只做 wire 形态校验：未知命令、未知应答种类、缺失的必填字段都会抛错，\n");
    out.push_str("// 界面不会拿到“看起来可用”的默认值。\n\n");

    push_local_ipc_constants_typescript(&mut out, model);
    push_ts_helpers(&mut out, &model.schemas);
    push_ts_schemas(&mut out, types, &model.schemas);
    trim_trailing_blank_line(&mut out);
    out
}

/// 本机 IPC 的协议版本与帧上限常量（TypeScript）。
fn push_local_ipc_constants_typescript(out: &mut String, model: &LocalIpcModel) {
    let IpcProtocol { major, minor } = model.protocol;
    out.push_str("/** agent 支持的 IPC 协议 major 版本（major 不同即不兼容）。 */\n");
    out.push_str(&format!("export const PROTOCOL_MAJOR = {major};\n\n"));
    out.push_str("/** agent 支持的 IPC 协议 minor 版本（高于此值的调用方被拒绝）。 */\n");
    out.push_str(&format!("export const PROTOCOL_MINOR = {minor};\n\n"));
    out.push_str("/** 帧长度头的字节数：大端无符号整数，表示后续 JSON 负载的字节数。 */\n");
    out.push_str(&format!(
        "export const FRAME_HEADER_BYTES = {};\n\n",
        model.frame.header_bytes
    ));
    out.push_str("/** 单帧负载上限（字节）：超过即拒绝，调用方与 agent 用同一个上限。 */\n");
    out.push_str(&format!(
        "export const MAX_FRAME_BYTES = {};\n\n",
        model.frame.max_frame_bytes
    ));
}

/// 按需生成解码辅助函数（两种契约共用）。
fn push_ts_helpers(out: &mut String, schemas: &BTreeMap<String, SchemaDef>) {
    for helper in used_helpers(schemas) {
        out.push_str(helper_source(helper));
    }
}

/// 把 schema 列表渲染成 TypeScript 接口、字符串字面量联合与解码函数（两种契约共用）。
fn push_ts_schemas(out: &mut String, types: &TargetTypes, schemas: &BTreeMap<String, SchemaDef>) {
    for (name, definition) in schemas {
        match definition {
            SchemaDef::Struct {
                doc,
                fields,
                closed,
            } => {
                push_ts_doc(out, "", doc);
                out.push_str(&format!("export interface {name} {{\n"));
                for field in fields {
                    push_ts_doc(out, "  ", &field.doc);
                    let ty = ts_type(types, &field.ty);
                    let marker = if field.optional { "?" } else { "" };
                    out.push_str(&format!("  {}{marker}: {ty};\n", field.wire));
                }
                out.push_str("}\n\n");
                out.push_str(&format!(
                    "export function decode{name}(value: unknown): {name} {{\n  const object = expect_object(value, \"{name}\");\n"
                ));
                if *closed {
                    let allowed = fields
                        .iter()
                        .map(|field| format!("\"{}\"", field.wire))
                        .collect::<Vec<_>>()
                        .join(", ");
                    out.push_str(&format!("  if (Object.keys(object).some((key) => ![{allowed}].includes(key))) {{\n    throw new Error(\"{name} contains unknown fields\");\n  }}\n"));
                }
                out.push_str("  return {\n");
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
                push_ts_doc(out, "", doc);
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
}

/// 渲染 TypeScript 端点调用文件。
fn render_typescript_client(config: &GenerationConfig, model: &Model) -> String {
    let mut out = String::new();
    out.push_str(&format!("// {}\n//\n", do_not_edit(&config.source)));
    push_wrapped(&mut out, "//", &provenance(model));
    push_description(&mut out, "//", &model.description);
    out.push_str("//\n");
    out.push_str(
        "// 传输细节（fetch、request_id）由调用方注入，客户端不直接读全局对象；非 2xx 响应按错误\n",
    );
    out.push_str("// 信封解码成 ApiError，未知错误码与畸形响应抛 ProtocolError。\n\n");

    let mut decoders = vec![format!("decode{}", super::ERROR_SCHEMA)];
    let mut types = vec![error_body_type(model)];
    for operation in &model.operations {
        if let SuccessBody::Json(name) = &operation.success {
            decoders.push(format!("decode{name}"));
            types.push(name.clone());
        }
        if let Some(request_type) = operation
            .request
            .as_ref()
            .and_then(RequestBodyKind::json_schema)
        {
            types.push(request_type.clone());
        }
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

function request_headers(
  options: ClientOptions,
  init: RequestInit | undefined,
  content_type: string | null,
  accept = \"application/json\",
): Headers {
  const headers = new Headers(init?.headers);
  headers.set(\"accept\", accept);
  if (content_type !== null) {
    headers.set(\"content-type\", content_type);
  }
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

async function read_bytes(response: Response): Promise<Uint8Array> {
  return new Uint8Array(await response.arrayBuffer());
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

    // 签名：`options` → 路径参数（按路径顺序）→ 请求头参数（按声明顺序）→ 请求体（有则带）→ `init`。
    let mut params = vec!["options: ClientOptions".to_string()];
    for param in &operation.path_params {
        params.push(format!("{}: string", param.name));
    }
    for param in &operation.header_params {
        params.push(format!("{}: string", param.ts_name));
    }
    // 查询参数收成一个对象实参：必填字段不带 `?`，可选字段带 `?`；字段名就是 wire 名。
    if !operation.query_params.is_empty() {
        let fields: Vec<String> = operation
            .query_params
            .iter()
            .map(|param| {
                let optional = if param.required { "" } else { "?" };
                format!("{}{optional}: string", param.name)
            })
            .collect();
        let all_optional = operation.query_params.iter().all(|param| !param.required);
        let optional = if all_optional { "?" } else { "" };
        params.push(format!("query{optional}: {{ {} }}", fields.join("; ")));
    }
    // 生成的实参：无请求体是 `null`，有请求体是带引号的媒体类型字面量。
    let mut content_type = "null";
    let mut body_line = String::new();
    match &operation.request {
        Some(RequestBodyKind::Json(schema)) => {
            params.push(format!("body: {schema}"));
            content_type = "\"application/json\"";
            body_line.push_str("    body: JSON.stringify(body),\n");
        }
        Some(RequestBodyKind::Binary) => {
            params.push("body: Uint8Array".to_string());
            content_type = "\"application/octet-stream\"";
            body_line.push_str("    body: body as BodyInit,\n");
        }
        None => {}
    }
    params.push("init?: RequestInit".to_string());
    let success_type = match &operation.success {
        SuccessBody::Json(name) => name.clone(),
        SuccessBody::Bytes => "Uint8Array".to_string(),
        SuccessBody::EventStream => "Response".to_string(),
    };
    out.push_str(&format!(
        "export async function {}({}): Promise<{}> {{\n",
        operation.operation_id,
        params.join(", "),
        success_type
    ));

    // 路径：占位符换成模板字面量，值用 `encodeURIComponent` 编码，不拼未转义文本。
    let mut url_expr = format!("\"{}\"", operation.path);
    if !operation.path_params.is_empty() {
        let mut template = String::new();
        let mut rest = operation.path.as_str();
        while let Some(open) = rest.find('{') {
            template.push_str(&rest[..open]);
            let after = &rest[open + 1..];
            let close = after.find('}').expect("生成模型已校验占位符闭合");
            let name = &after[..close];
            template.push_str(&format!("${{encodeURIComponent({name})}}"));
            rest = &after[close + 1..];
        }
        template.push_str(rest);
        out.push_str(&format!("  const path = `{template}`;\n"));
        url_expr = "path".to_string();
    }

    // 查询串：必填参数总是写入；可选参数只在调用方给了值时写入，不发送空值。
    if !operation.query_params.is_empty() {
        out.push_str("  const search = new URLSearchParams();\n");
        let all_optional = operation.query_params.iter().all(|param| !param.required);
        for param in &operation.query_params {
            if param.required {
                out.push_str(&format!("  search.set(\"{0}\", query.{0});\n", param.name));
            } else if all_optional {
                out.push_str(&format!(
                    "  if (query?.{0} !== undefined) {{\n    search.set(\"{0}\", query.{0});\n  }}\n",
                    param.name
                ));
            } else {
                out.push_str(&format!(
                    "  if (query.{0} !== undefined) {{\n    search.set(\"{0}\", query.{0});\n  }}\n",
                    param.name
                ));
            }
        }
        out.push_str("  const query_string = search.toString();\n");
        out.push_str(&format!(
            "  const target = query_string === \"\" ? {url_expr} : `${{{url_expr}}}?${{query_string}}`;\n"
        ));
        url_expr = "target".to_string();
    }

    // 字节响应意味着“读字节”，请求头也要声明接受字节（不假装在要 JSON）。
    let accept = match &operation.success {
        SuccessBody::Json(_) => "application/json",
        SuccessBody::Bytes => "application/octet-stream",
        SuccessBody::EventStream => "text/event-stream",
    };
    let headers_line = if operation.header_params.is_empty() {
        format!("    headers: request_headers(options, init, {content_type}, \"{accept}\"),\n")
    } else {
        out.push_str(&format!(
            "  const headers = request_headers(options, init, {content_type}, \"{accept}\");\n"
        ));
        for param in &operation.header_params {
            out.push_str(&format!(
                "  headers.set(\"{}\", {});\n",
                param.name, param.ts_name
            ));
        }
        "    headers,\n".to_string()
    };

    out.push_str(&format!(
        "  const response = await options.fetch(join_url(options.base_url, {url_expr}), {{\n    ...init,\n    method: \"{}\",\n{headers_line}{body_line}  }});\n",
        operation.method
    ));
    match &operation.success {
        SuccessBody::Json(name) => {
            out.push_str("  const payload = await read_json(response);\n");
            out.push_str(
                "  if (!response.ok) {\n    throw api_error(response.status, payload);\n  }\n",
            );
            out.push_str(&format!("  return decode{name}(payload);\n}}\n\n"));
        }
        SuccessBody::Bytes => {
            // 非 2xx 仍然是 JSON 错误信封；2xx 才是字节体。
            out.push_str(
                "  if (!response.ok) {\n    throw api_error(response.status, await read_json(response));\n  }\n",
            );
            out.push_str("  return await read_bytes(response);\n}\n\n");
        }
        SuccessBody::EventStream => {
            // 事件流由调用方按 SSE 解析（`response.body`）；非 2xx 仍是 JSON 错误信封。
            out.push_str(
                "  if (!response.ok) {\n    throw api_error(response.status, await read_json(response));\n  }\n",
            );
            out.push_str("  return response;\n}\n\n");
        }
    }
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
fn used_helpers(schemas: &BTreeMap<String, SchemaDef>) -> Vec<&'static str> {
    let mut used: Vec<&'static str> = Vec::new();
    for definition in schemas.values() {
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
fn schemas_use_map(schemas: &BTreeMap<String, SchemaDef>) -> bool {
    fn uses_map(ty: &FieldType) -> bool {
        match ty {
            FieldType::Map(_) => true,
            FieldType::Array(inner) => uses_map(inner),
            _ => false,
        }
    }
    schemas.values().any(|definition| match definition {
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

/// 按显示宽度折行（ASCII 记 1、其他字符记 2），优先在空白处分行；只有单个词本身超过整行
/// 宽度时才硬断，避免把标识符或路径从中间切开。本函数只用于表头来源行。
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    for word in text.split(' ') {
        let word_width = display_width(word);
        let needed = current_width + if current.is_empty() { 0 } else { 1 } + word_width;
        if !current.is_empty() && needed > width {
            lines.push(std::mem::take(&mut current));
            current_width = 0;
        }
        if word_width <= width {
            if !current.is_empty() {
                current.push(' ');
                current_width += 1;
            }
            current.push_str(word);
            current_width += word_width;
            continue;
        }
        for ch in word.chars() {
            let ch_width = display_width(&ch.to_string());
            if current_width + ch_width > width && !current.is_empty() {
                lines.push(std::mem::take(&mut current));
                current_width = 0;
            }
            current.push(ch);
            current_width += ch_width;
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// 粗略显示宽度：ASCII 记 1、其他字符记 2，够用即可，不做排版级精确对齐。
fn display_width(text: &str) -> usize {
    text.chars()
        .map(|ch| if ch.is_ascii() { 1 } else { 2 })
        .sum()
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
