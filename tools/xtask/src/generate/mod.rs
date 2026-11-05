//! 协议生成（`docs/design.md` §5.6、§5.12）：`api/openapi.yaml` → 远端 Rust DTO/TS 客户端，
//! `api/local-ipc.schema.json` → 本机 IPC Rust DTO/TS 类型。
//!
//! 两份契约各有唯一编辑源，互不引用：远端 HTTP 契约是 OpenAPI，本机 IPC 契约是 JSON Schema，
//! 服务器管理 API 与本机 IPC 因此不会悄悄合并成一套。本模块只负责把已冻结的 wire 契约翻译成
//! 两侧代码，并保证“重新生成逐字节一致”。生成器刻意只支持契约当前用到的构造：遇到未支持的
//! 关键字、未具名的内联对象、缺描述的字段都是错误退出，而不是猜一个默认值——静默降级会把
//! “契约里有、代码里没有”的问题拖到运行时。
//!
//! `api/generation.toml` 声明生成器身份、配置格式版本、两份源文件、输出路径与类型映射；每次
//! 运行先核对这些声明，配置与实现漂移就报错（避免出现过期的生成配置还以为在起作用）。

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::{EXIT_FAILURE, EXIT_OK, EXIT_USAGE, workspace_root};

mod emit;
mod local_ipc;

/// 生成配置格式版本；与 `api/generation.toml` 的 `format_version` 核对。
///
/// v3：端点可有 `requestBody`（只接受 `application/json` 且引用具名 schema）。
const FORMAT_VERSION: u32 = 3;
/// 生成器名；与 `api/generation.toml` 的 `generator` 核对。
const GENERATOR: &str = "vaultsyncd-xtask";
/// 生成配置文件（相对 workspace 根）。
const CONFIG_PATH: &str = "api/generation.toml";
/// 当前唯一支持的 OpenAPI 文档版本。
const OPENAPI_VERSION: &str = "3.1.0";
/// 错误信封请求的 schema 名：所有非 2xx 响应都必须引用它。
const ERROR_SCHEMA: &str = "ErrorEnvelope";
/// OpenAPI 契约的引用前缀（JSON Schema 契约的引用前缀在 [`local_ipc`] 里）。
const REF_PREFIX: &str = "#/components/schemas/";
/// 当前唯一支持的 JSON Schema 文档元数据：只认 [`local_ipc::SCHEMA_DRAFT`]。
const LOCAL_IPC_SCHEMA_DRAFT: &str = local_ipc::SCHEMA_DRAFT;

/// `cargo xtask generate [--check]`。
///
/// 不带参数时把生成结果写入配置声明的路径；`--check` 只比对内存结果与磁盘内容，用于 CI 与
/// “重新生成无 diff”的验收。
pub fn run(rest: &[String]) -> u8 {
    let check_only = match rest {
        [] => false,
        [flag] if flag == "--check" => true,
        _ => {
            eprintln!(
                "xtask: `generate` 只接受可选参数 `--check`（收到 `{}`）",
                rest.join(" ")
            );
            eprintln!("运行 `cargo xtask help` 查看可用子命令。");
            return EXIT_USAGE;
        }
    };
    match generate(check_only) {
        Ok(()) => EXIT_OK,
        Err(message) => {
            eprintln!("xtask: 协议生成失败：{message}");
            EXIT_FAILURE
        }
    }
}

/// 供 `check` 复用：只比对生成结果与磁盘，不写文件。
pub(crate) fn verify() -> Result<(), String> {
    generate(true)
}

/// 生成文件表头里禁止手改的说明；`source` 是契约源路径（两份契约各自写明自己的源）。
pub(super) fn do_not_edit(source: &str) -> String {
    format!("本文件由 `cargo xtask generate` 从 `{source}` 生成，禁止手改。")
}

/// 读取配置与两份契约、生成全部代码，然后写入或比对。
fn generate(check_only: bool) -> Result<(), String> {
    let root = workspace_root()?;
    let config = read_config(&root)?;
    let document = read_document(&root, &config)?;
    let model = Model::build(&document)?;
    let local_model = local_ipc::load(&root, &config)?;
    let mut outputs = emit::render(&config, &model);
    outputs.extend(emit::render_local_ipc(&config, &local_model));

    let mut changed = Vec::new();
    for (relative, content) in &outputs {
        let path = root.join(relative);
        let existing = fs::read_to_string(&path).ok();
        if existing.as_deref() == Some(content.as_str()) {
            continue;
        }
        changed.push(relative.clone());
        if !check_only {
            write_generated(&path, content)?;
        }
    }

    if check_only {
        if changed.is_empty() {
            println!("协议生成结果与磁盘一致（{} 个文件）", outputs.len());
            return Ok(());
        }
        for relative in &changed {
            eprintln!("  不一致：{relative}");
        }
        return Err(format!(
            "重新生成有 diff：{} 个文件与契约源不同步；运行 `cargo xtask generate` 后提交",
            changed.len()
        ));
    }

    if changed.is_empty() {
        println!("协议生成结果无变化（{} 个文件）", outputs.len());
    } else {
        for relative in &changed {
            println!("  写入：{relative}");
        }
        println!(
            "已生成 {} 个文件，其中 {} 个更新",
            outputs.len(),
            changed.len()
        );
    }
    Ok(())
}

/// 写生成文件；父目录不存在时创建，避免要求事先手工建目录。
fn write_generated(path: &Path, content: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("无法创建目录 {}：{error}", parent.display()))?;
    }
    fs::write(path, content).map_err(|error| format!("无法写入 {}：{error}", path.display()))
}

/// 生成配置（`api/generation.toml`）。字段即配置文件的全部内容，未知键解析失败。
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct GenerationConfig {
    /// 配置格式版本，必须等于 [`FORMAT_VERSION`]。
    pub(crate) format_version: u32,
    /// 生成器名，必须是 [`GENERATOR`]。
    pub(crate) generator: String,
    /// 生成器包版本，必须等于本二进制编译时的包版本。
    pub(crate) generator_version: String,
    /// 远端契约源文件（相对 workspace 根）。
    pub(crate) source: String,
    /// 远端契约声明的 OpenAPI 版本，必须与文档的 `openapi` 字段一致。
    pub(crate) openapi_version: String,
    /// 本机 IPC 契约源文件（相对 workspace 根）。
    pub(crate) local_ipc_source: String,
    /// 本机 IPC 契约声明的 JSON Schema draft，必须与文档的 `$schema` 一致。
    pub(crate) local_ipc_schema_draft: String,
    /// 生成代码路径。
    pub(crate) outputs: Outputs,
    /// wire 类型到目标语言的映射。
    pub(crate) types: TypeMapping,
}

/// 生成代码的输出路径，全部相对 workspace 根。
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Outputs {
    /// 远端 Rust DTO 文件。
    pub(crate) rust: String,
    /// 远端 TypeScript 类型与解码器文件。
    pub(crate) typescript_types: String,
    /// 远端 TypeScript HTTP 端点调用文件。
    pub(crate) typescript_client: String,
    /// 本机 IPC 的 Rust DTO 文件。
    pub(crate) local_ipc_rust: String,
    /// 本机 IPC 的 TypeScript 类型与解码器文件。
    pub(crate) local_ipc_typescript_types: String,
}

/// 两种目标语言的映射表。
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TypeMapping {
    /// Rust 映射。
    pub(crate) rust: TargetTypes,
    /// TypeScript 映射。
    pub(crate) typescript: TargetTypes,
}

/// 单个目标语言的映射；`array` 用 `{item}`、`map` 用 `{value}` 占位。
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TargetTypes {
    /// OpenAPI `boolean`。
    pub(crate) boolean: String,
    /// OpenAPI `integer`。
    pub(crate) integer: String,
    /// OpenAPI `string`（无自定义 format）。
    pub(crate) string: String,
    /// `format: decimal_u64` 的十进制字符串。
    pub(crate) decimal_u64: String,
    /// 数组模板。
    pub(crate) array: String,
    /// 字符串键映射模板。
    pub(crate) map: String,
}

/// 读并核对生成配置。
fn read_config(root: &Path) -> Result<GenerationConfig, String> {
    let path = root.join(CONFIG_PATH);
    let text = fs::read_to_string(&path)
        .map_err(|error| format!("无法读取 {}：{error}", path.display()))?;
    let config: GenerationConfig =
        toml::from_str(&text).map_err(|error| format!("{CONFIG_PATH} 解析失败：{error}"))?;

    if config.format_version != FORMAT_VERSION {
        return Err(format!(
            "{CONFIG_PATH} 的 format_version 是 {}，本生成器实现的是 {FORMAT_VERSION}",
            config.format_version
        ));
    }
    if config.generator != GENERATOR {
        return Err(format!(
            "{CONFIG_PATH} 的 generator 是 `{}`，本命令是 `{GENERATOR}`",
            config.generator
        ));
    }
    if config.generator_version != env!("CARGO_PKG_VERSION") {
        return Err(format!(
            "{CONFIG_PATH} 的 generator_version 是 {}，编译进本命令的是 {}；\
             生成器行为变化时同步更新版本与 format_version",
            config.generator_version,
            env!("CARGO_PKG_VERSION")
        ));
    }
    if config.openapi_version != OPENAPI_VERSION {
        return Err(format!(
            "{CONFIG_PATH} 的 openapi_version 是 {}，本生成器支持 {OPENAPI_VERSION}",
            config.openapi_version
        ));
    }
    if config.local_ipc_schema_draft != LOCAL_IPC_SCHEMA_DRAFT {
        return Err(format!(
            "{CONFIG_PATH} 的 local_ipc_schema_draft 是 {}，本生成器支持 {LOCAL_IPC_SCHEMA_DRAFT}",
            config.local_ipc_schema_draft
        ));
    }
    Ok(config)
}

/// 读契约并解析成原始文档结构。
fn read_document(root: &Path, config: &GenerationConfig) -> Result<OpenApiDocument, String> {
    let path = root.join(&config.source);
    let text = fs::read_to_string(&path)
        .map_err(|error| format!("无法读取契约 {}：{error}", path.display()))?;
    let document: OpenApiDocument = serde_yaml_ng::from_str(&text)
        .map_err(|error| format!("{} 解析失败：{error}", config.source))?;
    if document.openapi != config.openapi_version {
        return Err(format!(
            "{} 声明 openapi {}，与 {CONFIG_PATH} 的 {} 不一致",
            config.source, document.openapi, config.openapi_version
        ));
    }
    Ok(document)
}

/// 原始 OpenAPI 文档；只声明生成器读取的部分，其他键一律解析失败。
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenApiDocument {
    /// 文档版本，必须等于 [`OPENAPI_VERSION`]。
    openapi: String,
    /// 文档信息。
    info: Info,
    /// 路径 → 方法集合。
    paths: BTreeMap<String, PathItem>,
    /// 组件（当前只用 `schemas`）。
    components: Components,
}

/// `info` 对象。
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Info {
    /// 契约标题。
    title: String,
    /// 契约版本。
    version: String,
    /// 契约说明（进生成文件表头）。
    #[serde(default)]
    description: Option<String>,
}

/// `components` 对象；当前只支持 `schemas`。
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Components {
    /// 具名 schema。
    schemas: BTreeMap<String, RawSchema>,
}

/// 一个路径下的方法集合；方法名在 [`Model::build`] 中校验。
#[derive(Debug, serde::Deserialize)]
struct PathItem {
    /// 方法名（`get`、`post` 等）→ 操作。
    #[serde(flatten)]
    operations: BTreeMap<String, Operation>,
}

/// 一个 HTTP 操作。
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Operation {
    /// 端点标识，同时是 TypeScript 函数名。
    #[serde(rename = "operationId")]
    operation_id: String,
    /// 一行摘要（进生成代码文档）。
    summary: String,
    /// 补充说明（进生成代码文档）。
    #[serde(default)]
    description: Option<String>,
    /// 状态码 → 响应；`default` 之类非数字状态码被拒绝。
    responses: BTreeMap<String, Response>,
    /// 请求体；只在带请求体的端点出现（S043 起支持）。
    #[serde(rename = "requestBody", default)]
    request_body: Option<RequestBody>,
}

/// 一个请求体：只支持 `application/json`，且必须引用具名 schema（不生成内联结构）。
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestBody {
    /// 是否必填；缺省视为必填（`false` 表示请求体可以没有，当前契约不使用）。
    #[serde(default)]
    required: Option<bool>,
    /// 媒体类型 → schema；只接受 `application/json`。
    content: BTreeMap<String, MediaType>,
}

/// 一个响应。
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    /// 响应说明（进生成代码文档）。
    description: String,
    /// 响应体；本契约要求每个响应都有 JSON 体。
    #[serde(default)]
    content: Option<BTreeMap<String, MediaType>>,
}

/// 一个媒体类型。
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct MediaType {
    /// 媒体类型对应的 schema。
    schema: RawSchema,
}

/// 原始 schema 节点；所有 `Option` 字段在 [`Model::build`] 里按上下文校验。
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSchema {
    /// 说明；具名 schema 与属性必须有。
    #[serde(default)]
    description: Option<String>,
    /// `type`；只接受字符串形式。
    #[serde(rename = "type", default)]
    ty: Option<String>,
    /// `format`；只有 `decimal_u64` 被支持。
    #[serde(default)]
    format: Option<String>,
    /// 闭集枚举取值。
    #[serde(rename = "enum", default)]
    enum_values: Option<Vec<String>>,
    /// 具名对象的属性。
    #[serde(default)]
    properties: Option<BTreeMap<String, RawSchema>>,
    /// 必填属性名。
    #[serde(default)]
    required: Option<Vec<String>>,
    /// 数组元素。
    #[serde(default)]
    items: Option<Box<RawSchema>>,
    /// 映射值类型。
    #[serde(rename = "additionalProperties", default)]
    additional_properties: Option<Box<RawSchema>>,
    /// 整数下界（只进文档）。
    #[serde(default)]
    minimum: Option<i64>,
    /// 整数上界（只进文档）。
    #[serde(default)]
    maximum: Option<i64>,
    /// 组件引用；只接受 `#/components/schemas/<名字>`。
    #[serde(rename = "$ref", default)]
    reference: Option<String>,
}

/// 校验后的契约模型：只保留生成需要的构造。
#[derive(Debug)]
pub(crate) struct Model {
    /// 契约标题（进表头）。
    pub(crate) title: String,
    /// 契约版本（进表头）。
    pub(crate) version: String,
    /// 契约说明的各行原文（进表头，不重排，避免中文断行处出现多余空格）。
    pub(crate) description: Vec<String>,
    /// 具名 schema，按名字排序，保证输出稳定。
    pub(crate) schemas: BTreeMap<String, SchemaDef>,
    /// 端点，按路径与方法排序。
    pub(crate) operations: Vec<ApiOperation>,
}

/// 一个具名 schema。
#[derive(Debug)]
pub(crate) enum SchemaDef {
    /// 对象：生成结构体/接口 + 解码器。
    Struct {
        /// 说明。
        doc: String,
        /// 字段，按 wire 名字排序。
        fields: Vec<Field>,
    },
    /// 闭集枚举：未知取值必须解析失败。
    Enum {
        /// 说明。
        doc: String,
        /// 取值与对应变体名，按契约顺序。
        variants: Vec<Variant>,
    },
}

/// 结构体字段。
#[derive(Debug)]
pub(crate) struct Field {
    /// wire 名字（snake_case）。
    pub(crate) wire: String,
    /// 说明。
    pub(crate) doc: String,
    /// 字段类型。
    pub(crate) ty: FieldType,
    /// 是否可省略（不在 `required` 中）。
    pub(crate) optional: bool,
}

/// 字段类型。
#[derive(Debug)]
pub(crate) enum FieldType {
    /// `boolean`。
    Boolean,
    /// `integer`。
    Integer,
    /// `string`。
    Text,
    /// `format: decimal_u64` 的十进制文本。
    Decimal,
    /// 具名 schema 引用。
    Named(String),
    /// 数组。
    Array(Box<FieldType>),
    /// 字符串键映射。
    Map(Box<FieldType>),
}

/// 枚举取值。
#[derive(Debug)]
pub(crate) struct Variant {
    /// wire 取值。
    pub(crate) value: String,
    /// 生成代码里的变体名。
    pub(crate) name: String,
}

/// 一个端点。
#[derive(Debug)]
pub(crate) struct ApiOperation {
    /// 方法（大写）。
    pub(crate) method: String,
    /// 路径。
    pub(crate) path: String,
    /// 端点标识，同时是 TypeScript 函数名。
    pub(crate) operation_id: String,
    /// 文档行（摘要 + 补充说明 + 各状态码说明）。
    pub(crate) doc: Vec<String>,
    /// 2xx 响应的 schema 名。
    pub(crate) success_type: String,
    /// 请求体的 schema 名；`None` 表示无请求体。
    pub(crate) request_type: Option<String>,
}

impl Model {
    /// 把原始文档校验并折叠成生成模型；任何未支持构造都返回错误。
    fn build(document: &OpenApiDocument) -> Result<Self, String> {
        let mut schemas = BTreeMap::new();
        for (name, raw) in &document.components.schemas {
            validate_schema_name(name)?;
            schemas.insert(name.clone(), build_schema_def(name, raw, REF_PREFIX)?);
        }

        let mut operations = Vec::new();
        for (path, item) in &document.paths {
            for (method, operation) in &item.operations {
                operations.push(build_operation(path, method, operation)?);
            }
        }
        if operations.is_empty() {
            return Err("契约没有任何端点：`paths` 为空".to_string());
        }

        let model = Self {
            title: document.info.title.clone(),
            version: document.info.version.clone(),
            description: document
                .info
                .description
                .as_deref()
                .map(|text| {
                    text.lines()
                        .map(|line| line.trim_end().to_string())
                        .collect()
                })
                .unwrap_or_default(),
            schemas,
            operations,
        };
        model.validate_references()?;
        Ok(model)
    }

    /// 校验所有具名引用都存在，避免生成引用不到的类型。
    fn validate_references(&self) -> Result<(), String> {
        validate_schema_references(&self.schemas)?;
        for operation in &self.operations {
            if !self.schemas.contains_key(&operation.success_type) {
                return Err(format!(
                    "端点 {} 引用了未定义的 schema `{}`（{REF_PREFIX}{}）",
                    operation.operation_id, operation.success_type, operation.success_type
                ));
            }
            if let Some(request_type) = &operation.request_type {
                if !self.schemas.contains_key(request_type) {
                    return Err(format!(
                        "端点 {} 的请求体引用了未定义的 schema `{request_type}`（{REF_PREFIX}…）",
                        operation.operation_id
                    ));
                }
            }
        }
        Ok(())
    }
}

/// 校验所有具名引用都存在，避免生成引用不到的类型。
fn validate_schema_references(schemas: &BTreeMap<String, SchemaDef>) -> Result<(), String> {
    let check = |name: &str, context: &str| {
        if schemas.contains_key(name) {
            Ok(())
        } else {
            Err(format!(
                "{context} 引用了未定义的 schema `{name}`（{REF_PREFIX}{name}）"
            ))
        }
    };
    for (name, def) in schemas {
        let SchemaDef::Struct { fields, .. } = def else {
            continue;
        };
        for field in fields {
            check_field_type(
                &field.ty,
                &format!("schema {name} 的字段 {}", field.wire),
                &check,
            )?;
        }
    }
    Ok(())
}

/// 递归校验字段类型里的具名引用。
fn check_field_type(
    ty: &FieldType,
    context: &str,
    check: &impl Fn(&str, &str) -> Result<(), String>,
) -> Result<(), String> {
    match ty {
        FieldType::Named(name) => check(name, context),
        FieldType::Array(inner) | FieldType::Map(inner) => check_field_type(inner, context, check),
        FieldType::Boolean | FieldType::Integer | FieldType::Text | FieldType::Decimal => Ok(()),
    }
}

/// schema 名会直接成为 Rust/TypeScript 类型名，必须是 PascalCase 的 ASCII 标识符。
fn validate_schema_name(name: &str) -> Result<(), String> {
    let mut chars = name.chars();
    let valid_start = chars.next().is_some_and(|first| first.is_ascii_uppercase());
    let valid_rest = chars.all(|next| next.is_ascii_alphanumeric());
    if !valid_start || !valid_rest {
        return Err(format!(
            "schema 名 `{name}` 不是 PascalCase 的 ASCII 标识符；它既是生成的类型名也是函数名"
        ));
    }
    Ok(())
}

/// 校验单个具名 schema。`ref_prefix` 是该契约格式的引用前缀（OpenAPI 的
/// `#/components/schemas/` 或 JSON Schema 的 `#/$defs/`），两种契约共用同一份校验规则。
fn build_schema_def(name: &str, raw: &RawSchema, ref_prefix: &str) -> Result<SchemaDef, String> {
    if raw.reference.is_some() {
        return Err(format!(
            "schema `{name}` 不能是纯 `$ref` 别名；请直接引用目标类型"
        ));
    }
    let doc = require_doc(&format!("schema `{name}`"), raw.description.as_deref())?;

    if let Some(values) = &raw.enum_values {
        if raw.ty.as_deref() != Some("string") || raw.format.is_some() {
            return Err(format!(
                "schema `{name}` 的 `enum` 只支持无 format 的 string"
            ));
        }
        if raw.properties.is_some() || raw.items.is_some() || raw.additional_properties.is_some() {
            return Err(format!(
                "schema `{name}` 的 `enum` 不能同时声明 properties/items"
            ));
        }
        if values.is_empty() {
            return Err(format!("schema `{name}` 的 `enum` 不能为空"));
        }
        let mut variants: Vec<Variant> = Vec::new();
        for value in values {
            let variant = Variant {
                value: value.clone(),
                name: variant_name(value)
                    .ok_or_else(|| format!("schema `{name}` 的枚举值 `{value}` 不是合法标识符"))?,
            };
            if let Some(existing) = variants.iter().find(|other| other.name == variant.name) {
                return Err(format!(
                    "schema `{name}` 的枚举值 `{}` 与 `{}` 生成同一个变体名 `{}`",
                    variant.value, existing.value, variant.name
                ));
            }
            variants.push(variant);
        }
        return Ok(SchemaDef::Enum { doc, variants });
    }

    if raw.ty.as_deref() != Some("object") {
        return Err(format!(
            "schema `{name}` 只支持 object 或 string enum；当前 `type` 是 {:?}",
            raw.ty
        ));
    }
    if raw.additional_properties.is_some() {
        return Err(format!(
            "schema `{name}` 是 object：映射请写成字段的 `additionalProperties`，不要当作具名 schema"
        ));
    }
    let properties = raw
        .properties
        .as_ref()
        .ok_or_else(|| format!("schema `{name}` 是 object 但没有 properties"))?;
    if properties.is_empty() {
        return Err(format!("schema `{name}` 的 properties 不能为空"));
    }

    let required = raw.required.clone().unwrap_or_default();
    for field in &required {
        if !properties.contains_key(field) {
            return Err(format!(
                "schema `{name}` 的 required 列出未定义属性 `{field}`"
            ));
        }
    }

    let mut fields = Vec::new();
    for (wire, property) in properties {
        validate_field_name(name, wire)?;
        let context = format!("schema `{name}` 的字段 `{wire}`");
        let mut doc = require_doc(&context, property.description.as_deref())?;
        let ty = build_field_type(&context, property, ref_prefix)?;
        if matches!(ty, FieldType::Integer) {
            if property.minimum.is_some() || property.maximum.is_some() {
                doc.push_str(&format!("（取值范围 {}）", integer_range(property)));
            }
        } else if property.minimum.is_some() || property.maximum.is_some() {
            return Err(format!("{context} 的 minimum/maximum 只支持 integer"));
        }
        fields.push(Field {
            wire: wire.clone(),
            doc,
            ty,
            optional: !required.contains(wire),
        });
    }
    Ok(SchemaDef::Struct { doc, fields })
}

/// 校验字段类型的构造。
fn build_field_type(context: &str, raw: &RawSchema, ref_prefix: &str) -> Result<FieldType, String> {
    if let Some(reference) = &raw.reference {
        let name = reference
            .strip_prefix(ref_prefix)
            .ok_or_else(|| format!("{context} 的 `$ref` 只支持 {ref_prefix}…：{reference}"))?;
        return Ok(FieldType::Named(name.to_string()));
    }
    if raw.enum_values.is_some() {
        return Err(format!("{context} 使用内联 `enum`；请提成具名 schema"));
    }
    match raw.ty.as_deref() {
        Some("string") => match raw.format.as_deref() {
            None => Ok(FieldType::Text),
            Some("decimal_u64") => Ok(FieldType::Decimal),
            Some(other) => Err(format!(
                "{context} 的 format `{other}` 未支持；目前只有 decimal_u64"
            )),
        },
        Some("boolean") => {
            reject_format(context, raw)?;
            Ok(FieldType::Boolean)
        }
        Some("integer") => {
            reject_format(context, raw)?;
            Ok(FieldType::Integer)
        }
        Some("array") => {
            reject_format(context, raw)?;
            let items = raw
                .items
                .as_ref()
                .ok_or_else(|| format!("{context} 是 array 但没有 items"))?;
            Ok(FieldType::Array(Box::new(build_field_type(
                &format!("{context}[]"),
                items,
                ref_prefix,
            )?)))
        }
        Some("object") => {
            reject_format(context, raw)?;
            if raw.properties.is_some() {
                return Err(format!(
                    "{context} 是内联对象；请提成具名 schema，否则无法生成稳定类型"
                ));
            }
            let values = raw
                .additional_properties
                .as_ref()
                .ok_or_else(|| format!("{context} 是 object 但没有 additionalProperties"))?;
            let value_type = build_field_type(&format!("{context} 的值"), values, ref_prefix)?;
            match value_type {
                FieldType::Text => Ok(FieldType::Map(Box::new(FieldType::Text))),
                _ => Err(format!(
                    "{context} 的映射值只支持 string；当前是 {value_type:?}"
                )),
            }
        }
        other => Err(format!(
            "{context} 的 `type` 是 {other:?}；未支持（只有 boolean/integer/string/array/映射）"
        )),
    }
}

/// 整数上下界的可读描述（只进生成的文档，不改变类型）。
fn integer_range(raw: &RawSchema) -> String {
    let min = raw
        .minimum
        .map(|value| value.to_string())
        .unwrap_or_else(|| "无下界".to_string());
    let max = raw
        .maximum
        .map(|value| value.to_string())
        .unwrap_or_else(|| "无上界".to_string());
    format!("{min}..={max}")
}

/// 拒绝在非 string 字段上出现的 `format`。
fn reject_format(context: &str, raw: &RawSchema) -> Result<(), String> {
    match raw.format {
        Some(ref format) => Err(format!("{context} 的 format `{format}` 未支持")),
        None => Ok(()),
    }
}

/// 要求有说明：生成代码的公开类型与字段必须带文档，`missing_docs` 才能在两侧生效。
fn require_doc(context: &str, description: Option<&str>) -> Result<String, String> {
    let text = description.unwrap_or_default().trim();
    if text.is_empty() {
        return Err(format!(
            "{context} 缺少 description：生成代码要求每个公开类型与字段有说明"
        ));
    }
    Ok(text.to_string())
}

/// 字段名必须是 snake_case，避免 wire 与 Rust/TS 命名约定分叉。
fn validate_field_name(schema: &str, wire: &str) -> Result<(), String> {
    let mut chars = wire.chars();
    let valid_start = chars.next().is_some_and(|first| first.is_ascii_lowercase());
    let valid_rest =
        chars.all(|next| next.is_ascii_lowercase() || next.is_ascii_digit() || next == '_');
    if !valid_start || !valid_rest {
        return Err(format!(
            "schema `{schema}` 的字段名 `{wire}` 不是 snake_case；wire 与生成代码共用该名字"
        ));
    }
    if RUST_KEYWORDS.contains(&wire) {
        return Err(format!(
            "schema `{schema}` 的字段名 `{wire}` 与 Rust 关键字冲突；请在契约里改名"
        ));
    }
    Ok(())
}

/// 与 Rust 冲突的关键字：生成代码不额外做转义，遇到就要求契约改名。
const RUST_KEYWORDS: &[&str] = &[
    "as", "break", "const", "continue", "crate", "else", "enum", "extern", "false", "fn", "for",
    "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return",
    "self", "static", "struct", "super", "trait", "true", "type", "unsafe", "use", "where",
    "while", "async", "await", "dyn", "abstract", "become", "box", "do", "final", "macro",
    "override", "priv", "typeof", "unsized", "virtual", "yield", "try",
];

/// 把 wire 取值转成 PascalCase 变体名；只接受 ASCII 字母、数字与 `_`。
fn variant_name(value: &str) -> Option<String> {
    let mut name = String::new();
    for part in value.split('_') {
        if part.is_empty() || !part.chars().all(|ch| ch.is_ascii_alphanumeric()) {
            return None;
        }
        let mut chars = part.chars();
        let first = chars.next()?;
        if first.is_ascii_digit() {
            return None;
        }
        name.extend(first.to_uppercase());
        name.extend(chars.flat_map(char::to_lowercase));
    }
    if name.is_empty() { None } else { Some(name) }
}

/// 校验并折叠一个端点。
fn build_operation(
    path: &str,
    method: &str,
    operation: &Operation,
) -> Result<ApiOperation, String> {
    let method_upper = match method {
        "get" => "GET",
        "post" => "POST",
        "put" => "PUT",
        "delete" => "DELETE",
        other => {
            return Err(format!(
                "路径 {path} 的方法 `{other}` 未支持；只有 get/post/put/delete"
            ));
        }
    };
    validate_operation_id(path, &operation.operation_id)?;

    let mut doc = vec![operation.summary.trim().to_string()];
    if let Some(description) = &operation.description {
        let text = description.trim();
        if !text.is_empty() {
            doc.push(String::new());
            doc.extend(text.lines().map(|line| line.trim_end().to_string()));
        }
    }
    doc.push(String::new());

    let mut success_type = None;
    for (status, response) in &operation.responses {
        let code: u16 = status.parse().map_err(|_| {
            format!(
                "端点 {} 的状态码 `{status}` 不是数字（不支持 default）",
                operation.operation_id
            )
        })?;
        let schema = response
            .content
            .as_ref()
            .and_then(|content| content.get("application/json"))
            .ok_or_else(|| {
                format!(
                    "端点 {} 的状态码 {code} 缺少 application/json 响应体",
                    operation.operation_id
                )
            })?;
        let name = schema
            .schema
            .reference
            .as_deref()
            .and_then(|reference| reference.strip_prefix(REF_PREFIX))
            .ok_or_else(|| {
                format!(
                    "端点 {} 的状态码 {code} 必须引用具名 schema（$ref: {REF_PREFIX}…）",
                    operation.operation_id
                )
            })?;
        doc.push(format!("{code}：{}", response.description.trim()));

        if (200..300).contains(&code) {
            if success_type.is_some() {
                return Err(format!(
                    "端点 {} 有多个 2xx 响应；生成器只支持一个成功响应",
                    operation.operation_id
                ));
            }
            success_type = Some(name.to_string());
        } else if name != ERROR_SCHEMA {
            return Err(format!(
                "端点 {} 的状态码 {code} 引用 `{name}`；非 2xx 响应必须是 {ERROR_SCHEMA}",
                operation.operation_id
            ));
        }
    }

    Ok(ApiOperation {
        method: method_upper.to_string(),
        path: path.to_string(),
        operation_id: operation.operation_id.clone(),
        doc,
        success_type: success_type
            .ok_or_else(|| format!("端点 {} 没有 2xx 响应", operation.operation_id))?,
        request_type: build_request_type(operation)?,
    })
}

/// 解析请求体：只接受 `application/json` + 具名 schema 引用。
fn build_request_type(operation: &Operation) -> Result<Option<String>, String> {
    let Some(body) = &operation.request_body else {
        return Ok(None);
    };
    if body.required == Some(false) {
        return Err(format!(
            "端点 {} 的 requestBody 声明为可选；生成器只支持必填请求体",
            operation.operation_id
        ));
    }
    if body.content.len() != 1 || !body.content.contains_key("application/json") {
        return Err(format!(
            "端点 {} 的 requestBody 只支持 application/json（实有：{}）",
            operation.operation_id,
            body.content.keys().cloned().collect::<Vec<_>>().join("、")
        ));
    }
    let media = &body.content["application/json"];
    let reference = media.schema.reference.as_deref().ok_or_else(|| {
        format!(
            "端点 {} 的请求体必须引用具名 schema（$ref: {REF_PREFIX}…），不要写内联对象",
            operation.operation_id
        )
    })?;
    let name = reference
        .strip_prefix(REF_PREFIX)
        .ok_or_else(|| format!("端点 {} 的请求体 `$ref` 只支持 {REF_PREFIX}…", operation.operation_id))?;
    Ok(Some(name.to_string()))
}

/// 端点标识会变成 TypeScript 函数名，必须是 lowerCamelCase。
fn validate_operation_id(path: &str, operation_id: &str) -> Result<(), String> {
    let mut chars = operation_id.chars();
    let valid_start = chars.next().is_some_and(|first| first.is_ascii_lowercase());
    let valid_rest = chars.all(|next| next.is_ascii_alphanumeric());
    if !valid_start || !valid_rest {
        return Err(format!(
            "路径 {path} 的 operationId `{operation_id}` 不是 lowerCamelCase；它也是生成的函数名"
        ));
    }
    Ok(())
}

/// 供 emit 读取的只读视图：契约标题与版本。
impl Model {
    /// 生成文件表头用的来源描述。
    pub(crate) fn source_line(&self) -> String {
        format!("{} {}", self.title, self.version)
    }
}

#[cfg(test)]
mod tests {
    use super::{Model, RUST_KEYWORDS, variant_name};

    #[test]
    fn variant_names_follow_wire_values() {
        assert_eq!(variant_name("ok").as_deref(), Some("Ok"));
        assert_eq!(
            variant_name("needs_recovery").as_deref(),
            Some("NeedsRecovery")
        );
        assert_eq!(variant_name("AUTH_EXPIRED").as_deref(), Some("AuthExpired"));
        assert_eq!(
            variant_name("UNSUPPORTED_STORAGE_BACKEND").as_deref(),
            Some("UnsupportedStorageBackend")
        );
        assert!(variant_name("").is_none());
        assert!(variant_name("1_starts_with_digit").is_none());
        assert!(variant_name("has-dash").is_none());
    }

    #[test]
    fn keyword_list_contains_rust_reserved_words() {
        assert!(RUST_KEYWORDS.contains(&"type"));
        assert!(!RUST_KEYWORDS.contains(&"ok"));
    }

    #[test]
    fn model_rejects_unimplemented_constructs() {
        // 内联 enum 必须提成具名 schema：直接用最小文档验证错误路径，而不是靠人工 review。
        let yaml = r##"
openapi: "3.1.0"
info:
  title: t
  version: "1"
paths:
  /v1/x:
    get:
      operationId: getX
      summary: s
      responses:
        "200":
          description: d
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Thing"}
components:
  schemas:
    Thing:
      description: 有说明
      type: object
      required: [mode]
      properties:
        mode:
          type: string
          description: 内联枚举样本
          enum: [a, b]
"##;
        let document: super::OpenApiDocument =
            serde_yaml_ng::from_str(yaml).expect("fixture 可解析");
        let error = Model::build(&document).expect_err("内联 enum 必须报错");
        assert!(
            error.contains("内联 `enum`"),
            "错误应指出内联 enum，实际：{error}"
        );
    }

    #[test]
    fn model_rejects_missing_reference() {
        let yaml = r##"
openapi: "3.1.0"
info:
  title: t
  version: "1"
paths:
  /v1/x:
    get:
      operationId: getX
      summary: s
      responses:
        "200":
          description: d
          content:
            application/json:
              schema: {$ref: "#/components/schemas/Missing"}
components:
  schemas: {}
"##;
        let document: super::OpenApiDocument =
            serde_yaml_ng::from_str(yaml).expect("fixture 可解析");
        let error = Model::build(&document).expect_err("缺引用必须报错");
        assert!(
            error.contains("未定义的 schema"),
            "错误应指出未定义引用，实际：{error}"
        );
    }
}
