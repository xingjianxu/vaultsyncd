//! 本机 IPC 契约（`api/local-ipc.schema.json`，JSON Schema）→ Rust DTO 与 TypeScript 类型。
//!
//! 与远端契约分开（design §5.6、§15.5）：本机 IPC 不经网络传输，也不复用 `api/openapi.yaml`
//! 的端点与错误信封。这里只做“契约 → 类型”的翻译，帧与命令处理器在 `crates/local-ipc` 内实现。
//!
//! 本契约比远端契约多两处约定，它们在文档头的 `x-vaultsyncd` 里声明并由生成器校验：
//!
//! - `protocol`：agent 与 GUI/CLI 协商的 IPC 协议主次版本；
//! - `frame`：帧长度头的字节数与单帧负载上限（`framing` 用它们做有界读取）。
//!
//! 生成器只支持当前契约用到的构造：对象、闭集字符串枚举、`#/$defs/…` 引用、布尔/整数/字符串、
//! 数组与字符串映射。遇到未支持的构造、缺描述的字段、非法 `x-vaultsyncd` 一律报错退出。

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use super::{
    GenerationConfig, RawSchema, SchemaDef, build_schema_def, validate_schema_name,
    validate_schema_references,
};

/// 本契约声明的 JSON Schema draft（与 `api/generation.toml` 的 `local_ipc_schema_draft` 核对）。
pub(super) const SCHEMA_DRAFT: &str = "2020-12";
/// JSON Schema 的引用前缀；只接受同一文件内的具名定义。
const REF_PREFIX: &str = "#/$defs/";
/// 长度头的字节数：只有 4（大端 `u32`）被实现；换宽度必须同时改 `framing`。
const FRAME_HEADER_BYTES: u32 = 4;
/// 单帧负载上限的允许区间：太小装不下正常应答，太大则失去“有界内存”的意义。
const MIN_MAX_FRAME_BYTES: u32 = 64;
/// 单帧负载上限的最大允许值（1 GiB 与 64 MiB 之间取 64 MiB，避免一个坏帧就能耗尽内存）。
const MAX_MAX_FRAME_BYTES: u32 = 64 * 1024 * 1024;

/// 读契约文件并校验成生成模型。
pub(super) fn load(root: &Path, config: &GenerationConfig) -> Result<LocalIpcModel, String> {
    let path = root.join(&config.local_ipc_source);
    let text = fs::read_to_string(&path)
        .map_err(|error| format!("无法读取本机 IPC 契约 {}：{error}", path.display()))?;
    let document: LocalIpcDocument = serde_json::from_str(&text)
        .map_err(|error| format!("{} 解析失败：{error}", config.local_ipc_source))?;
    if !document
        .schema
        .ends_with(&format!("draft/{SCHEMA_DRAFT}/schema"))
    {
        return Err(format!(
            "{} 声明 $schema {}，与 {SCHEMA_DRAFT} 不一致",
            config.local_ipc_source, document.schema
        ));
    }
    build(config, document)
}

/// 原始 JSON Schema 文档；只声明生成器读取的部分，其他键一律解析失败。
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalIpcDocument {
    /// `$schema`：必须指向 [`SCHEMA_DRAFT`]。
    #[serde(rename = "$schema")]
    schema: String,
    /// 契约标题（进生成文件表头）。
    title: String,
    /// 契约说明；按源文件断行原样进表头。
    description: String,
    /// 本契约独有的协议版本与帧上限。
    #[serde(rename = "x-vaultsyncd")]
    meta: LocalIpcMeta,
    /// 具名定义。
    #[serde(rename = "$defs")]
    defs: BTreeMap<String, RawSchema>,
}

/// `x-vaultsyncd` 元数据。
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalIpcMeta {
    /// IPC 协议版本。
    protocol: ProtocolSpec,
    /// 帧格式上限。
    frame: FrameSpec,
}

/// 协议版本声明。
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ProtocolSpec {
    /// 不兼容版本号。
    major: u32,
    /// 兼容增量版本号。
    minor: u32,
}

/// 帧格式声明。
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FrameSpec {
    /// 长度头字节数。
    header_bytes: u32,
    /// 单帧负载上限（字节）。
    max_frame_bytes: u32,
}

/// 校验后的本机 IPC 契约模型。
#[derive(Debug)]
pub(crate) struct LocalIpcModel {
    /// 契约标题（进表头）。
    pub(crate) title: String,
    /// 契约说明各行（进表头，不重排，避免中文断行处出现多余空格）。
    pub(crate) description: Vec<String>,
    /// agent 支持的 IPC 协议版本。
    pub(crate) protocol: IpcProtocol,
    /// 帧格式上限。
    pub(crate) frame: FrameLimits,
    /// 具名 schema，按名字排序，保证输出稳定。
    pub(crate) schemas: BTreeMap<String, SchemaDef>,
}

/// 已校验的 IPC 协议版本。
#[derive(Debug, Clone, Copy)]
pub(crate) struct IpcProtocol {
    /// 不兼容版本号。
    pub(crate) major: u32,
    /// 兼容增量版本号。
    pub(crate) minor: u32,
}

/// 已校验的帧格式上限。
#[derive(Debug, Clone, Copy)]
pub(crate) struct FrameLimits {
    /// 长度头字节数。
    pub(crate) header_bytes: u32,
    /// 单帧负载上限（字节）。
    pub(crate) max_frame_bytes: u32,
}

/// 构建并校验模型；任何未支持构造都返回错误。
fn build(config: &GenerationConfig, document: LocalIpcDocument) -> Result<LocalIpcModel, String> {
    let context = &config.local_ipc_source;
    validate_meta(context, &document.meta)?;

    let mut schemas = BTreeMap::new();
    for (name, raw) in &document.defs {
        validate_schema_name(name)?;
        schemas.insert(name.clone(), build_schema_def(name, raw, REF_PREFIX)?);
    }
    if schemas.is_empty() {
        return Err(format!("{context} 的 `$defs` 为空：没有可生成的类型"));
    }
    validate_schema_references(&schemas)?;

    Ok(LocalIpcModel {
        title: document.title,
        description: document
            .description
            .lines()
            .map(|line| line.trim_end().to_string())
            .collect(),
        protocol: IpcProtocol {
            major: document.meta.protocol.major,
            minor: document.meta.protocol.minor,
        },
        frame: FrameLimits {
            header_bytes: document.meta.frame.header_bytes,
            max_frame_bytes: document.meta.frame.max_frame_bytes,
        },
        schemas,
    })
}

/// 校验 `x-vaultsyncd`：版本号与帧上限都会进生成常量，不合理的取值必须在这里失败。
fn validate_meta(context: &str, meta: &LocalIpcMeta) -> Result<(), String> {
    if meta.protocol.major == 0 {
        return Err(format!(
            "{context} 的 x-vaultsyncd.protocol.major 不能是 0：major 变化才表示不兼容变更"
        ));
    }
    if meta.protocol.minor > 65535 || meta.protocol.major > 65535 {
        return Err(format!(
            "{context} 的 x-vaultsyncd.protocol 版本号超过 65535，与 IpcVersion 字段范围不一致"
        ));
    }
    if meta.frame.header_bytes != FRAME_HEADER_BYTES {
        return Err(format!(
            "{context} 的 x-vaultsyncd.frame.header_bytes 是 {}，本生成器与 framing 实现的是 {FRAME_HEADER_BYTES}",
            meta.frame.header_bytes
        ));
    }
    if !(MIN_MAX_FRAME_BYTES..=MAX_MAX_FRAME_BYTES).contains(&meta.frame.max_frame_bytes) {
        return Err(format!(
            "{context} 的 x-vaultsyncd.frame.max_frame_bytes 是 {}，必须在 {MIN_MAX_FRAME_BYTES}..={MAX_MAX_FRAME_BYTES} 之间",
            meta.frame.max_frame_bytes
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{LocalIpcDocument, build};
    use crate::generate::GenerationConfig;

    /// 生成配置的测试替身：只需要契约路径用于错误信息。
    fn config() -> GenerationConfig {
        let parsed: GenerationConfig = toml::from_str(
            r#"
format_version = 2
generator = "vaultsyncd-xtask"
generator_version = "0.1.0"
source = "api/openapi.yaml"
openapi_version = "3.1.0"
local_ipc_source = "api/local-ipc.schema.json"
local_ipc_schema_draft = "2020-12"

[outputs]
rust = "a"
typescript_types = "b"
typescript_client = "c"
local_ipc_rust = "d"
local_ipc_typescript_types = "e"

[types.rust]
boolean = "bool"
integer = "u32"
string = "String"
decimal_u64 = "String"
array = "Vec<{item}>"
map = "BTreeMap<String, {value}>"

[types.typescript]
boolean = "boolean"
integer = "number"
string = "string"
decimal_u64 = "string"
array = "Array<{item}>"
map = "Record<string, {value}>"
"#,
        )
        .expect("测试配置必须可解析");
        parsed
    }

    /// 一份最小的合法文档：一个对象 schema 引用一个枚举 schema。
    const MINIMAL: &str = r##"
{
  "$schema": "https://json-schema.org/draft/2020-12/schema",
  "title": "t",
  "description": "d",
  "x-vaultsyncd": {
    "protocol": {"major": 1, "minor": 0},
    "frame": {"header_bytes": 4, "max_frame_bytes": 4096}
  },
  "$defs": {
    "Mode": {
      "type": "string",
      "description": "模式",
      "enum": ["a", "b"]
    },
    "Thing": {
      "type": "object",
      "description": "东西",
      "required": ["mode"],
      "properties": {
        "mode": {"$ref": "#/$defs/Mode", "description": "模式"}
      }
    }
  }
}
"##;

    #[test]
    fn minimal_document_builds_model() {
        let document: LocalIpcDocument = serde_json::from_str(MINIMAL).expect("fixture 可解析");
        let model = build(&config(), document).expect("合法文档必须通过");
        assert_eq!(model.protocol.major, 1);
        assert_eq!(model.protocol.minor, 0);
        assert_eq!(model.frame.header_bytes, 4);
        assert_eq!(model.frame.max_frame_bytes, 4096);
        assert_eq!(model.schemas.len(), 2);
    }

    #[test]
    fn frame_header_width_is_fixed() {
        let document: LocalIpcDocument =
            serde_json::from_str(&MINIMAL.replace(r#""header_bytes": 4"#, r#""header_bytes": 8"#))
                .expect("fixture 可解析");
        let error = build(&config(), document).expect_err("宽度不符必须报错");
        assert!(error.contains("header_bytes"), "实际错误：{error}");
    }

    #[test]
    fn frame_limit_out_of_range_is_rejected() {
        for value in ["16", "999999999"] {
            let text = MINIMAL.replace(
                r#""max_frame_bytes": 4096"#,
                &format!(r#""max_frame_bytes": {value}"#),
            );
            let document: LocalIpcDocument = serde_json::from_str(&text).expect("fixture 可解析");
            let error = build(&config(), document).expect_err("上限越界必须报错");
            assert!(error.contains("max_frame_bytes"), "实际错误：{error}");
        }
    }

    #[test]
    fn version_zero_is_rejected() {
        let text = MINIMAL.replace(r#""major": 1"#, r#""major": 0"#);
        let document: LocalIpcDocument = serde_json::from_str(&text).expect("fixture 可解析");
        let error = build(&config(), document).expect_err("major 0 必须报错");
        assert!(error.contains("major"), "实际错误：{error}");
    }

    #[test]
    fn reference_outside_defs_is_rejected() {
        let text = MINIMAL.replace("#/$defs/Mode", "#/components/schemas/Mode");
        let document: LocalIpcDocument = serde_json::from_str(&text).expect("fixture 可解析");
        let error = build(&config(), document).expect_err("跨文件引用必须报错");
        assert!(error.contains("$defs"), "实际错误：{error}");
    }

    #[test]
    fn unknown_top_level_key_is_rejected() {
        let text = MINIMAL.replace(r#""title": "t","#, r#""title": "t", "examples": {},"#);
        let error = serde_json::from_str::<LocalIpcDocument>(&text).expect_err("未知键必须报错");
        assert!(
            error.to_string().contains("unknown field"),
            "实际错误：{error}"
        );
    }
}
