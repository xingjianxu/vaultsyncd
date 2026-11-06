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
/// v4：端点的成功响应可以是二进制切片（`application/octet-stream`，无 schema），参数可以是
/// `in: header` 的必填字符串。
/// v5：参数可以是 `in: query`（标量 `string`/`decimal_u64`，必填或可选），成功响应可以是
/// `text/event-stream`（SSE，无 schema，TypeScript 侧返回原始 `Response` 由调用方读流）。
const FORMAT_VERSION: u32 = 5;
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
    /// 参数（S045 起支持路径参数，S046-b 起支持请求头参数，S048 起支持查询参数）；只接受 `in: path`、
    /// 必填的 `in: header` 与 `in: query`。
    #[serde(default)]
    parameters: Vec<Parameter>,
    /// 状态码 → 响应；`default` 之类非数字状态码被拒绝。
    responses: BTreeMap<String, Response>,
    /// 请求体；只在带请求体的端点出现（S043 起支持 JSON，S045 起支持二进制块）。
    #[serde(rename = "requestBody", default)]
    request_body: Option<RequestBody>,
}

/// 一个端点参数：路径参数（`in: path`）或必填请求头参数（`in: header`）。
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Parameter {
    /// 参数名；路径参数必须与路径里的 `{name}` 占位符一致，头参数是头名。
    name: String,
    /// 参数位置；只接受 `path`、`header` 与 `query`。
    #[serde(rename = "in")]
    location: String,
    /// 是否必填；路径参数与请求头参数都必须显式 `required: true`。
    #[serde(default)]
    required: Option<bool>,
    /// 参数说明（进生成代码文档）。
    #[serde(default)]
    description: Option<String>,
    /// 参数值的 schema。
    schema: RawSchema,
}

/// 一个请求体：`application/json`（必须引用具名 schema）或 `application/octet-stream`（原始字节）。
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestBody {
    /// 是否必填；缺省视为必填（`false` 表示请求体可以没有，当前契约不使用）。
    #[serde(default)]
    required: Option<bool>,
    /// 媒体类型 → 请求体内容。
    content: BTreeMap<String, RequestMedia>,
}

/// 请求体的一个媒体类型。
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestMedia {
    /// JSON 请求体的 schema；二进制请求体不声明 schema。
    #[serde(default)]
    schema: Option<RawSchema>,
}

/// 一个响应。
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Response {
    /// 响应说明（进生成代码文档）。
    description: String,
    /// 响应体：2xx 是 JSON 具名 schema 或二进制切片，非 2xx 必须是 JSON 错误信封。
    #[serde(default)]
    content: Option<BTreeMap<String, MediaType>>,
}

/// 一个媒体类型。
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct MediaType {
    /// 媒体类型对应的 schema；二进制响应不声明 schema（形状由端点文档说明）。
    #[serde(default)]
    schema: Option<RawSchema>,
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
    additional_properties: Option<AdditionalProperties>,
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

/// Explicit object closure or a typed string-map value schema.
#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
enum AdditionalProperties {
    Allowed(bool),
    Schema(Box<RawSchema>),
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
        /// Unknown input fields are rejected only when the contract explicitly closes the object.
        closed: bool,
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
    /// 2xx 响应体。
    pub(crate) success: SuccessBody,
    /// 路径参数，按路径中的出现顺序。
    pub(crate) path_params: Vec<PathParam>,
    /// `in: header` 参数，按声明顺序。
    pub(crate) header_params: Vec<HeaderParam>,
    /// `in: query` 参数，按声明顺序。
    pub(crate) query_params: Vec<QueryParam>,
    /// 请求体；`None` 表示无请求体。
    pub(crate) request: Option<RequestBodyKind>,
}

/// 一次成功响应的形状。
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SuccessBody {
    /// `application/json` + 具名 schema：解析成生成类型。
    Json(String),
    /// `application/octet-stream`：原始字节，不解码、没有生成类型。
    Bytes,
    /// `text/event-stream`（SSE）：长连接事件流，不解码；TypeScript 侧交回原始 `Response`。
    EventStream,
}

impl SuccessBody {
    /// 两个成功响应是否声明了同一种响应体（同一端点可以有 200 与 206）。
    fn same_shape(&self, other: &Self) -> bool {
        self == other
    }
}

/// 一个 `in: header` 参数：wire 头名与 TypeScript 侧参数名。
#[derive(Debug)]
pub(crate) struct HeaderParam {
    /// HTTP 头名（小写，与 `headers.set` 的第一个实参一致）。
    pub(crate) name: String,
    /// TypeScript 参数名（`x-part-sha256` → `x_part_sha256`）。
    pub(crate) ts_name: String,
}

/// 一个 `in: query` 参数：名字与是否必填（值的 wire 类型只有 `string`/`decimal_u64`，TypeScript 里都是
/// `string`，解析时已校验，不另存）。
#[derive(Debug)]
pub(crate) struct QueryParam {
    /// 查询参数名（小写字母、数字与 `_`，同时是 TypeScript 对象字段名）。
    pub(crate) name: String,
    /// 是否必填。
    pub(crate) required: bool,
}

/// 路径参数：名字与值的 wire 类型（两者在 TypeScript 里都是 `string`）。
#[derive(Debug)]
pub(crate) struct PathParam {
    /// 参数名，与路径里的 `{name}` 一致。
    pub(crate) name: String,
    /// 值的 wire 类型（`string` 或 `decimal_u64`）。
    pub(crate) ty: FieldType,
}

/// 请求体类型。
#[derive(Debug)]
pub(crate) enum RequestBodyKind {
    /// `application/json` + 具名 schema：TypeScript 侧序列化对象。
    Json(String),
    /// `application/octet-stream`：TypeScript 侧直接发送字节。
    Binary,
}

impl RequestBodyKind {
    /// JSON 请求体的 schema 名；二进制请求体没有 schema。
    pub(crate) fn json_schema(&self) -> Option<&String> {
        match self {
            Self::Json(name) => Some(name),
            Self::Binary => None,
        }
    }
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
            let SuccessBody::Json(name) = &operation.success else {
                continue;
            };
            if !self.schemas.contains_key(name) {
                return Err(format!(
                    "端点 {} 引用了未定义的 schema `{name}`（{REF_PREFIX}{name}）",
                    operation.operation_id
                ));
            }
        }
        for operation in &self.operations {
            if let Some(request_type) = operation
                .request
                .as_ref()
                .and_then(RequestBodyKind::json_schema)
            {
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
    let closed = match &raw.additional_properties {
        None | Some(AdditionalProperties::Allowed(true)) => false,
        Some(AdditionalProperties::Allowed(false)) => true,
        Some(AdditionalProperties::Schema(_)) => {
            return Err(format!(
                "schema `{name}` 是 object：映射请写成字段的 `additionalProperties`，不要当作具名 schema"
            ));
        }
    };
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
    Ok(SchemaDef::Struct {
        doc,
        fields,
        closed,
    })
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
            let Some(AdditionalProperties::Schema(values)) = &raw.additional_properties else {
                return Err(format!(
                    "{context} 是映射但没有 additionalProperties 值 schema"
                ));
            };
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

    let path_params = build_path_params(path, &operation.parameters)?;
    let header_params = build_header_params(path, &operation.parameters)?;
    let query_params = build_query_params(path, &operation.parameters)?;
    let param_docs = describe_params(
        path,
        &operation.parameters,
        &path_params,
        &header_params,
        &query_params,
    );
    if !param_docs.is_empty() {
        doc.extend(param_docs);
        doc.push(String::new());
    }

    let mut success: Option<SuccessBody> = None;
    for (status, response) in &operation.responses {
        let code: u16 = status.parse().map_err(|_| {
            format!(
                "端点 {} 的状态码 `{status}` 不是数字（不支持 default）",
                operation.operation_id
            )
        })?;
        doc.push(format!("{code}：{}", response.description.trim()));

        if (200..300).contains(&code) {
            // 同一端点可以有 200 与 206（整份与区间切片），但形状必须一致：生成侧只有一个成功返回。
            let body = build_success_body(operation, code, response)?;
            match &success {
                Some(existing) if !existing.same_shape(&body) => {
                    return Err(format!(
                        "端点 {} 的 2xx 响应体形状不一致；同一端点的成功响应只能是 JSON 或字节",
                        operation.operation_id
                    ));
                }
                Some(_) => {}
                None => success = Some(body),
            }
            continue;
        }

        let name = json_schema_name(operation, code, response)?;
        if name != ERROR_SCHEMA {
            return Err(format!(
                "端点 {} 的状态码 {code} 引用 `{name}`；非 2xx 响应必须是 {ERROR_SCHEMA}",
                operation.operation_id
            ));
        }
    }

    Ok(ApiOperation {
        method: method_upper.to_string(),
        path: path.to_string(),
        path_params,
        header_params,
        query_params,
        operation_id: operation.operation_id.clone(),
        doc,
        success: success.ok_or_else(|| format!("端点 {} 没有 2xx 响应", operation.operation_id))?,
        request: build_request_body(operation)?,
    })
}

/// 一个响应的 `application/json` 具名 schema 名。
fn json_schema_name<'a>(
    operation: &Operation,
    code: u16,
    response: &'a Response,
) -> Result<&'a str, String> {
    let media = response
        .content
        .as_ref()
        .and_then(|content| content.get("application/json"))
        .ok_or_else(|| {
            format!(
                "端点 {} 的状态码 {code} 缺少 application/json 响应体",
                operation.operation_id
            )
        })?;
    media
        .schema
        .as_ref()
        .and_then(|schema| schema.reference.as_deref())
        .and_then(|reference| reference.strip_prefix(REF_PREFIX))
        .ok_or_else(|| {
            format!(
                "端点 {} 的状态码 {code} 必须引用具名 schema（$ref: {REF_PREFIX}…）",
                operation.operation_id
            )
        })
}

/// 一个 2xx 响应的响应体形状。
///
/// `application/json` 必须引用具名 schema；`application/octet-stream` 必须不声明 schema——
/// 二进制切片的形状（`Content-Range`、长度）写在端点文档里，生成器不猜一个类型。
fn build_success_body(
    operation: &Operation,
    code: u16,
    response: &Response,
) -> Result<SuccessBody, String> {
    let content = response.content.as_ref().ok_or_else(|| {
        format!(
            "端点 {} 的状态码 {code} 缺少响应体；成功响应必须声明 JSON 或字节",
            operation.operation_id
        )
    })?;
    if content.len() != 1 {
        return Err(format!(
            "端点 {} 的状态码 {code} 必须恰好声明一种媒体类型（实有：{}）",
            operation.operation_id,
            content.keys().cloned().collect::<Vec<_>>().join("、")
        ));
    }
    let (media_type, media) = content.iter().next().expect("上面刚检查过只有一个媒体类型");
    match media_type.as_str() {
        "application/json" => Ok(SuccessBody::Json(
            json_schema_name(operation, code, response)?.to_string(),
        )),
        "application/octet-stream" => {
            if media.schema.is_some() {
                return Err(format!(
                    "端点 {} 的状态码 {code} 的二进制响应不声明 schema",
                    operation.operation_id
                ));
            }
            Ok(SuccessBody::Bytes)
        }
        "text/event-stream" => {
            if media.schema.is_some() {
                return Err(format!(
                    "端点 {} 的状态码 {code} 的事件流响应不声明 schema",
                    operation.operation_id
                ));
            }
            Ok(SuccessBody::EventStream)
        }
        other => Err(format!(
            "端点 {} 的状态码 {code} 的媒体类型 `{other}` 未支持；只支持 application/json、application/octet-stream 与 text/event-stream",
            operation.operation_id
        )),
    }
}

/// 把路径参数与头参数的说明追加进端点文档行（生成的 TS 函数因此能说明每个参数是什么）。
fn describe_params(
    path: &str,
    parameters: &[Parameter],
    params: &[PathParam],
    header_params: &[HeaderParam],
    query_params: &[QueryParam],
) -> Vec<String> {
    let mut lines = describe_path_params(path, parameters, params);
    for param in query_params {
        let Some(raw) = parameters.iter().find(|raw| raw.name == param.name) else {
            continue;
        };
        let description = raw
            .description
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .unwrap_or("查询参数");
        let presence = if param.required { "必填" } else { "可选" };
        lines.push(format!(
            "查询参数 `{}`（{presence}）：{description}",
            param.name
        ));
    }
    for param in header_params {
        let Some(raw) = parameters.iter().find(|raw| raw.name == param.name) else {
            continue;
        };
        let description = raw
            .description
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .unwrap_or("请求头参数");
        lines.push(format!("请求头 `{}`：{description}", param.name));
    }
    lines
}

/// 把路径参数的说明追加进端点文档行（生成的 TS 函数因此能说明每个参数是什么）。
fn describe_path_params(path: &str, parameters: &[Parameter], params: &[PathParam]) -> Vec<String> {
    let mut lines = Vec::new();
    for param in params {
        let Some(raw) = parameters.iter().find(|raw| raw.name == param.name) else {
            continue;
        };
        let description = raw
            .description
            .as_deref()
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .unwrap_or("路径参数");
        lines.push(format!("参数 `{}`（{}）：{description}", param.name, path));
    }
    lines
}

/// 解析路径参数，并要求它与路径里的 `{name}` 占位符一一对应。
///
/// 只接受 `in: path` 的必填参数，且值的 wire 类型必须是 `string` 或 `format: decimal_u64`：
/// 两者在 TypeScript 侧都是 `string`，因此不会发明“数字参数”的第三种拼写。内联对象、
/// 其他位置（query/header，后两者分别在 `build_header_params` 与拒绝路径里）、可选参数、重复参数、
/// 未声明的占位符一律报错。
fn build_path_params(path: &str, parameters: &[Parameter]) -> Result<Vec<PathParam>, String> {
    let mut placeholders = Vec::new();
    let mut rest = path;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        let close = after
            .find('}')
            .ok_or_else(|| format!("路径 {path} 的占位符没有闭合的 `}}`"))?;
        let name = &after[..close];
        if name.is_empty() || name.contains('/') || name.contains('{') {
            return Err(format!("路径 {path} 的占位符 `{{{name}}}` 不是合法参数名"));
        }
        if placeholders.iter().any(|seen: &String| seen == name) {
            return Err(format!("路径 {path} 重复使用了占位符 `{name}`"));
        }
        placeholders.push(name.to_owned());
        rest = &after[close + 1..];
    }

    let mut params = Vec::new();
    for parameter in parameters {
        if parameter.location == "header" || parameter.location == "query" {
            continue;
        }
        if parameter.location != "path" {
            return Err(format!(
                "路径 {path} 的参数 `{}` 在 `{}` 位置；生成器只支持 `in: path`、`in: header` 与 `in: query`",
                parameter.name, parameter.location
            ));
        }
        if parameter.required != Some(true) {
            return Err(format!(
                "路径 {path} 的参数 `{}` 必须显式 `required: true`",
                parameter.name
            ));
        }
        let ty = parameter_type(path, parameter)?;
        if !placeholders.iter().any(|name| name == &parameter.name) {
            return Err(format!(
                "路径 {path} 声明了参数 `{}`，但路径里没有 `{{{}}}` 占位符",
                parameter.name, parameter.name
            ));
        }
        if params
            .iter()
            .any(|seen: &PathParam| seen.name == parameter.name)
        {
            return Err(format!("路径 {path} 重复声明了参数 `{}`", parameter.name));
        }
        params.push(PathParam {
            name: parameter.name.clone(),
            ty,
        });
    }

    // 按路径中的出现顺序输出，生成函数的参数顺序因此稳定。
    let mut ordered = Vec::with_capacity(placeholders.len());
    for name in &placeholders {
        let param = params
            .iter()
            .find(|param| &param.name == name)
            .ok_or_else(|| format!("路径 {path} 的占位符 `{{{name}}}` 缺少 `parameters` 声明"))?;
        ordered.push(PathParam {
            name: param.name.clone(),
            ty: match &param.ty {
                FieldType::Decimal => FieldType::Decimal,
                _ => FieldType::Text,
            },
        });
    }
    Ok(ordered)
}

/// 解析 `in: header` 参数。
///
/// 只接受必填的 `type: string` 参数，且必须是客户端不自己设置的头（`accept`、`content-type`、
/// `x-request-id` 由运行时统一写入）。可选请求头（如 `Range`）不生成参数，由调用方经
/// `init.headers` 传入——生成器不为同一个头发明两种拼写。
fn build_header_params(path: &str, parameters: &[Parameter]) -> Result<Vec<HeaderParam>, String> {
    let mut params: Vec<HeaderParam> = Vec::new();
    for parameter in parameters {
        if parameter.location != "header" {
            continue;
        }
        if parameter.required != Some(true) {
            return Err(format!(
                "路径 {path} 的请求头参数 `{}` 必须显式 `required: true`；可选请求头由调用方经 `init.headers` 传",
                parameter.name
            ));
        }
        let schema = &parameter.schema;
        if schema.reference.is_some()
            || schema.enum_values.is_some()
            || schema.properties.is_some()
            || schema.items.is_some()
            || schema.additional_properties.is_some()
            || schema.ty.as_deref() != Some("string")
            || schema.format.is_some()
        {
            return Err(format!(
                "路径 {path} 的请求头参数 `{}` 只支持无 format 的 `type: string`",
                parameter.name
            ));
        }
        let ts_name = header_param_ident(path, &parameter.name)?;
        if params.iter().any(|seen| seen.name == parameter.name) {
            return Err(format!(
                "路径 {path} 重复声明了请求头参数 `{}`",
                parameter.name
            ));
        }
        params.push(HeaderParam {
            name: parameter.name.clone(),
            ts_name,
        });
    }
    Ok(params)
}

/// 解析 `in: query` 参数：标量 `string`/`decimal_u64`，必填或可选（缺省视为可选），名字是小写
/// snake_case（同时是 TypeScript 对象字段名，不发明第二种拼写）。
fn build_query_params(path: &str, parameters: &[Parameter]) -> Result<Vec<QueryParam>, String> {
    let mut params: Vec<QueryParam> = Vec::new();
    for parameter in parameters {
        if parameter.location != "query" {
            continue;
        }
        let name = &parameter.name;
        let valid = name
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_lowercase())
            && name
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_');
        if !valid {
            return Err(format!(
                "路径 {path} 的查询参数 `{name}` 必须是小写 snake_case（字母开头，只含小写字母、数字与 `_`）"
            ));
        }
        if params.iter().any(|seen| &seen.name == name) {
            return Err(format!("路径 {path} 重复声明了查询参数 `{name}`"));
        }
        parameter_type(path, parameter)?;
        params.push(QueryParam {
            name: name.clone(),
            required: parameter.required == Some(true),
        });
    }
    Ok(params)
}

/// 运行时自己设置的头：契约再声明一次只会静默覆盖，因此直接报错。
const CLIENT_MANAGED_HEADERS: &[&str] = &["accept", "content-type", "x-request-id"];

/// 请求头名转成 TypeScript 参数名（`x-part-sha256` → `x_part_sha256`）。
fn header_param_ident(path: &str, name: &str) -> Result<String, String> {
    if name.is_empty()
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(format!(
            "路径 {path} 的请求头参数 `{name}` 不是小写 HTTP 头名（小写字母、数字与 `-`）"
        ));
    }
    if CLIENT_MANAGED_HEADERS.contains(&name) {
        return Err(format!(
            "路径 {path} 的请求头 `{name}` 由客户端运行时自己设置，不能作为参数声明"
        ));
    }
    let ident = name.replace('-', "_");
    let starts_ok = ident
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_lowercase());
    if !starts_ok {
        return Err(format!(
            "路径 {path} 的请求头参数 `{name}` 无法转成 TypeScript 标识符（`{ident}`）"
        ));
    }
    Ok(ident)
}

/// 路径参数的值类型：`string` 或 `format: decimal_u64`。
fn parameter_type(path: &str, parameter: &Parameter) -> Result<FieldType, String> {
    let schema = &parameter.schema;
    if schema.reference.is_some()
        || schema.enum_values.is_some()
        || schema.properties.is_some()
        || schema.items.is_some()
        || schema.additional_properties.is_some()
    {
        return Err(format!(
            "路径 {path} 的参数 `{}` 只支持标量 schema（`type: string` 或 `format: decimal_u64`）",
            parameter.name
        ));
    }
    match (schema.ty.as_deref(), schema.format.as_deref()) {
        (Some("string"), None | Some("decimal_u64")) => Ok(match schema.format.as_deref() {
            Some("decimal_u64") => FieldType::Decimal,
            _ => FieldType::Text,
        }),
        _ => Err(format!(
            "路径 {path} 的参数 `{}` 只支持 `type: string`（可带 `format: decimal_u64`）",
            parameter.name
        )),
    }
}

/// 解析请求体：`application/json`（具名 schema 引用）或 `application/octet-stream`（无 schema）。
fn build_request_body(operation: &Operation) -> Result<Option<RequestBodyKind>, String> {
    let Some(body) = &operation.request_body else {
        return Ok(None);
    };
    if body.required == Some(false) {
        return Err(format!(
            "端点 {} 的 requestBody 声明为可选；生成器只支持必填请求体",
            operation.operation_id
        ));
    }
    if body.content.len() != 1 {
        return Err(format!(
            "端点 {} 的 requestBody 必须恰好声明一种媒体类型（实有：{}）",
            operation.operation_id,
            body.content.keys().cloned().collect::<Vec<_>>().join("、")
        ));
    }
    let (media_type, media) = body
        .content
        .iter()
        .next()
        .expect("上面刚检查过只有一个媒体类型");
    match media_type.as_str() {
        "application/json" => {
            let schema = media.schema.as_ref().ok_or_else(|| {
                format!(
                    "端点 {} 的 JSON 请求体必须有 schema",
                    operation.operation_id
                )
            })?;
            let reference = schema.reference.as_deref().ok_or_else(|| {
                format!(
                    "端点 {} 的请求体必须引用具名 schema（$ref: {REF_PREFIX}…），不要写内联对象",
                    operation.operation_id
                )
            })?;
            let name = reference.strip_prefix(REF_PREFIX).ok_or_else(|| {
                format!(
                    "端点 {} 的请求体 `$ref` 只支持 {REF_PREFIX}…",
                    operation.operation_id
                )
            })?;
            Ok(Some(RequestBodyKind::Json(name.to_string())))
        }
        "application/octet-stream" => {
            if media.schema.is_some() {
                return Err(format!(
                    "端点 {} 的二进制请求体不声明 schema（内容语义由端点文档说明）",
                    operation.operation_id
                ));
            }
            Ok(Some(RequestBodyKind::Binary))
        }
        other => Err(format!(
            "端点 {} 的 requestBody 媒体类型 `{other}` 未支持；只支持 application/json 与 application/octet-stream",
            operation.operation_id
        )),
    }
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
    use super::{Model, RUST_KEYWORDS, SuccessBody, variant_name};

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

    /// 最小文档：一个端点，`operation` 文本直接拼进 `paths` 下的 `get`。
    fn model_with(operation: &str) -> Result<Model, String> {
        let yaml = format!(
            r##"
openapi: "3.1.0"
info:
  title: t
  version: "1"
paths:
  /v1/x:
    get:
      operationId: getX
      summary: s
{operation}
components:
  schemas:
    Thing:
      description: 有说明
      type: object
      required: [ok]
      properties:
        ok:
          type: boolean
          description: 样本字段
"##
        );
        let document: super::OpenApiDocument =
            serde_yaml_ng::from_str(&yaml).expect("fixture 可解析");
        Model::build(&document)
    }

    /// 成功响应声明：`body` 是 `content` 的缩进正文。
    fn responses(status: &str, body: &str) -> String {
        format!(
            "      responses:\n        \"{status}\":\n          description: d\n          content:\n{body}"
        )
    }

    const JSON_THING: &str = "            application/json:\n              schema: {$ref: \"#/components/schemas/Thing\"}\n";
    const OCTETS: &str = "            application/octet-stream: {}\n";

    #[test]
    fn binary_success_body_needs_no_schema() {
        let operation = responses("200", OCTETS);
        let model = model_with(&operation).expect("二进制响应应被接受");
        assert_eq!(model.operations[0].success, SuccessBody::Bytes);
    }

    #[test]
    fn ranged_binary_success_may_declare_206_of_the_same_shape() {
        // 整份与区间切片是两个状态码、同一种字节响应。
        let operation = format!(
            "      responses:\n        \"200\":\n          description: 整份\n          content:\n{OCTETS}        \"206\":\n          description: 切片\n          content:\n{OCTETS}"
        );
        let model = model_with(&operation).expect("200 与 206 同为字节应被接受");
        assert_eq!(model.operations[0].success, SuccessBody::Bytes);
    }

    #[test]
    fn conflicting_success_bodies_are_rejected() {
        let operation = format!(
            "      responses:\n        \"200\":\n          description: json\n          content:\n{JSON_THING}        \"206\":\n          description: 字节\n          content:\n{OCTETS}"
        );
        let error = model_with(&operation).expect_err("形状不一致必须报错");
        assert!(
            error.contains("响应体形状不一致"),
            "错误应指出形状不一致，实际：{error}"
        );
    }

    #[test]
    fn binary_success_body_must_not_carry_a_schema() {
        let operation = responses(
            "200",
            "            application/octet-stream:\n              schema: {$ref: \"#/components/schemas/Thing\"}\n",
        );
        let error = model_with(&operation).expect_err("二进制响应带 schema 必须报错");
        assert!(
            error.contains("二进制响应不声明 schema"),
            "错误应指出二进制响应不能有 schema，实际：{error}"
        );
    }

    #[test]
    fn header_parameters_become_typed_arguments() {
        let operation = format!(
            "      parameters:\n        - name: x-part-sha256\n          in: header\n          required: true\n          description: 块摘要\n          schema:\n            type: string\n{}",
            responses("200", JSON_THING)
        );
        let model = model_with(&operation).expect("请求头参数应被接受");
        let api = &model.operations[0];
        assert!(api.path_params.is_empty());
        assert_eq!(api.header_params.len(), 1);
        assert_eq!(api.header_params[0].name, "x-part-sha256");
        assert_eq!(api.header_params[0].ts_name, "x_part_sha256");
    }

    #[test]
    fn optional_or_client_managed_header_parameters_are_rejected() {
        for (extra, expected) in [
            (
                "        - name: range\n          in: header\n          description: 区间\n          schema:\n            type: string\n",
                "必须显式 `required: true`",
            ),
            (
                "        - name: x-request-id\n          in: header\n          required: true\n          description: 请求标识\n          schema:\n            type: string\n",
                "由客户端运行时自己设置",
            ),
            (
                "        - name: X-Bad\n          in: header\n          required: true\n          description: 非小写头名\n          schema:\n            type: string\n",
                "不是小写 HTTP 头名",
            ),
        ] {
            let operation = format!("      parameters:\n{extra}{}", responses("200", JSON_THING));
            let error = model_with(&operation).expect_err("必须拒绝该请求头参数");
            assert!(
                error.contains(expected),
                "期望错误含「{expected}」，实际：{error}"
            );
        }
    }

    #[test]
    fn query_parameters_become_a_typed_object_argument() {
        let operation = format!(
            "      parameters:\n        - name: after\n          in: query\n          required: true\n          description: 游标\n          schema:\n            type: string\n            format: decimal_u64\n        - name: limit\n          in: query\n          description: 页大小\n          schema:\n            type: string\n            format: decimal_u64\n{}",
            responses("200", JSON_THING)
        );
        let model = model_with(&operation).expect("查询参数应被接受");
        let api = &model.operations[0];
        assert_eq!(api.query_params.len(), 2);
        assert_eq!(api.query_params[0].name, "after");
        assert!(api.query_params[0].required);
        assert_eq!(api.query_params[1].name, "limit");
        assert!(!api.query_params[1].required, "缺省视为可选");
    }

    #[test]
    fn invalid_query_parameters_are_rejected() {
        for (extra, expected) in [
            (
                "        - name: After\n          in: query\n          description: 大写名\n          schema:\n            type: string\n",
                "小写 snake_case",
            ),
            (
                "        - name: a\n          in: query\n          description: 一\n          schema:\n            type: string\n        - name: a\n          in: query\n          description: 二\n          schema:\n            type: string\n",
                "重复声明了查询参数",
            ),
            (
                "        - name: a\n          in: query\n          description: 内联对象\n          schema:\n            type: object\n",
                "只支持 `type: string`",
            ),
            (
                "        - name: a\n          in: cookie\n          description: cookie\n          schema:\n            type: string\n",
                "只支持 `in: path`",
            ),
        ] {
            let operation = format!("      parameters:\n{extra}{}", responses("200", JSON_THING));
            let error = model_with(&operation).expect_err("必须拒绝该查询参数");
            assert!(
                error.contains(expected),
                "期望错误含「{expected}」，实际：{error}"
            );
        }
    }

    #[test]
    fn event_stream_success_body_needs_no_schema() {
        let operation = responses("200", "            text/event-stream: {}\n");
        let model = model_with(&operation).expect("事件流响应应被接受");
        assert_eq!(model.operations[0].success, SuccessBody::EventStream);

        let with_schema = responses(
            "200",
            "            text/event-stream:\n              schema: {$ref: \"#/components/schemas/Thing\"}\n",
        );
        let error = model_with(&with_schema).expect_err("事件流带 schema 必须报错");
        assert!(error.contains("事件流响应不声明 schema"), "{error}");
    }
}
