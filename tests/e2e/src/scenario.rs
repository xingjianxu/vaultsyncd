//! 数据驱动的 S074 三客户端场景描述（`tests/scenarios/*.json`）。
//!
//! 这一层是**纯数据模型**：不做 I/O，也不认识 Windows 平台调用，因此可以在 Linux 宿主用
//! `cargo test -p vaultsyncd-e2e` 校验场景文件本身。真正的跨进程编排在
//! [`crate::run`]（仅 Windows 编译）。
//!
//! 场景文件只描述“谁在什么时候写入哪份内容”；每类 block 的验收判据固定写在编排器里，
//! 避免把断言写成场景文件里的自由文本。新增同类数据只改 JSON，不改代码。

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// 场景文件只接受这一版 schema；形状变化必须显式升版，不能静默按旧规则执行。
pub const SCHEMA: u32 = 1;

/// 一份有确定文本内容的同步目录文件。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FileSpec {
    /// 相对同步根、以 `/` 分隔的路径。
    pub path: String,
    /// UTF-8 文本内容（每份输入内容都必须能在当前/冲突/历史中找到）。
    pub content: String,
}

/// 客户端身份与同步模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// 完整驻留：远端内容会下载成本地文件。
    Full,
    /// 按需：远端文件只保留占位，读取时才水合。
    OnDemand,
}

impl Mode {
    /// 配置文件里的模式文本。
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::OnDemand => "on_demand",
        }
    }
}

/// 一个客户端环境声明（独立 DeviceId/状态目录/同步根由编排器分配）。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClientSpec {
    /// 场景内引用名（小写字母、数字、`-`）。
    pub id: String,
    /// 服务端设备标签里显示的说明文字。
    pub label: String,
    /// 同步模式。
    pub mode: Mode,
}

/// 场景文件。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    /// 文件形状版本，必须是 [`SCHEMA`]。
    pub schema: u32,
    /// 稳定的场景身份。
    pub id: String,
    /// 可读标题。
    pub title: String,
    /// 本场景覆盖的 design §21 验收编号。
    pub requirements: Vec<String>,
    /// 参与者。
    pub clients: Vec<ClientSpec>,
    /// 客户端首次导入前服务端 Root 已有的非空内容。
    pub server_seed: Vec<FileSpec>,
    /// 按顺序执行的场景块。
    pub blocks: Vec<Block>,
}

/// 一个场景块。`kind` 直接决定编排器执行的验收判据。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Block {
    /// T01：三端分别非空导入，独有内容保留、同内容归并、异内容保留两份、不推导删除。
    Import {
        /// 块身份。
        id: String,
        /// 覆盖的验收编号。
        requirements: Vec<String>,
        /// 每个客户端绑定前的本地内容（单文件内容必须与服务端的 `shared-import.txt` 同文）。
        local: std::collections::BTreeMap<String, Vec<FileSpec>>,
    },
    /// T02：三端离线修改同一文件，再逐端接入。
    OfflineEdit {
        /// 块身份。
        id: String,
        /// 覆盖的验收编号。
        requirements: Vec<String>,
        /// 被三端离线修改的路径。
        path: String,
        /// 每端的离线修改内容。
        contents: std::collections::BTreeMap<String, String>,
    },
    /// T09：重命名保持条目身份，按需端只保留占位、不下载内容。
    Rename {
        /// 块身份。
        id: String,
        /// 覆盖的验收编号。
        requirements: Vec<String>,
        /// 执行重命名的客户端。
        renamer: String,
        /// 仅持有占位的客户端。
        placeholder_client: String,
        /// 原路径。
        from: String,
        /// 新路径。
        to: String,
    },
    /// T11：删除与离线修改并发。
    DeleteModify {
        /// 块身份。
        id: String,
        /// 覆盖的验收编号。
        requirements: Vec<String>,
        /// 被删除的路径。
        path: String,
        /// 删除该文件的客户端。
        deleter: String,
        /// 离线修改该文件的客户端。
        modifier: String,
        /// 离线修改内容。
        modified_content: String,
    },
    /// T03：提交成功但响应丢失，重试必须幂等、不产生重复内容、不复活已删除内容。
    LostResponse {
        /// 块身份。
        id: String,
        /// 覆盖的验收编号。
        requirements: Vec<String>,
        /// put 响应丢失。
        put: LostPut,
        /// delete 响应丢失。
        delete: LostDelete,
    },
}

/// put 响应丢失数据。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LostPut {
    /// 执行修改的客户端。
    pub client: String,
    /// 被修改的路径。
    pub path: String,
    /// 先同步的初始内容。
    pub initial_content: String,
    /// 丢响应那一次提交的新内容。
    pub content: String,
}

/// delete 响应丢失数据。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LostDelete {
    /// 执行删除的客户端。
    pub client: String,
    /// 被删除的路径。
    pub path: String,
    /// 删除前的内容。
    pub content: String,
}

impl Block {
    /// 块身份。
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::Import { id, .. }
            | Self::OfflineEdit { id, .. }
            | Self::Rename { id, .. }
            | Self::DeleteModify { id, .. }
            | Self::LostResponse { id, .. } => id,
        }
    }

    /// 覆盖的验收编号。
    #[must_use]
    pub fn requirements(&self) -> &[String] {
        match self {
            Self::Import { requirements, .. }
            | Self::OfflineEdit { requirements, .. }
            | Self::Rename { requirements, .. }
            | Self::DeleteModify { requirements, .. }
            | Self::LostResponse { requirements, .. } => requirements,
        }
    }
}

impl Scenario {
    /// 从 JSON 字节解析并做完整校验。
    ///
    /// # Errors
    ///
    /// JSON 形状错误或语义校验失败时返回可读原因。
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let scenario: Self =
            serde_json::from_slice(bytes).map_err(|error| format!("场景 JSON 无效：{error}"))?;
        scenario.validate()?;
        Ok(scenario)
    }

    /// 载入并解析一个场景文件。
    ///
    /// # Errors
    ///
    /// 读文件失败或校验失败。
    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        let bytes = std::fs::read(path)
            .map_err(|error| format!("读取场景 {} 失败：{error}", path.display()))?;
        Self::parse(&bytes)
    }

    /// 语义校验：引用、身份、路径与内容都必须明确，不做“尽量猜”的兜底。
    ///
    /// # Errors
    ///
    /// 任一约束不满足。
    pub fn validate(&self) -> Result<(), String> {
        if self.schema != SCHEMA {
            return Err(format!(
                "场景 schema 必须是 {SCHEMA}，实际是 {}",
                self.schema
            ));
        }
        require_token("场景 id", &self.id)?;
        if self.title.trim().is_empty() {
            return Err("场景 title 不能为空".into());
        }
        if self.requirements.is_empty() {
            return Err("场景 requirements 不能为空".into());
        }
        if self.clients.is_empty() {
            return Err("场景至少需要一个客户端".into());
        }
        let mut clients = BTreeSet::new();
        for client in &self.clients {
            require_token("客户端 id", &client.id)?;
            if !clients.insert(client.id.as_str()) {
                return Err(format!("客户端 id 重复：{}", client.id));
            }
            if client.label.trim().is_empty() {
                return Err(format!("客户端 {} 的 label 不能为空", client.id));
            }
        }
        unique_paths("server_seed", &self.server_seed)?;
        for file in &self.server_seed {
            require_content(&file.path, &file.content)?;
        }
        if self.blocks.is_empty() {
            return Err("场景至少需要一个 block".into());
        }
        let mut blocks = BTreeSet::new();
        for block in &self.blocks {
            if !blocks.insert(block.id()) {
                return Err(format!("block id 重复：{}", block.id()));
            }
            if block.requirements().is_empty() {
                return Err(format!("block {} 的 requirements 不能为空", block.id()));
            }
            match block {
                Block::Import { local, .. } => {
                    if local.is_empty() {
                        return Err("import block 的 local 不能为空".into());
                    }
                    for (client, files) in local {
                        known(&clients, client)?;
                        unique_paths(client, files)?;
                        for file in files {
                            require_content(&file.path, &file.content)?;
                        }
                    }
                }
                Block::OfflineEdit { path, contents, .. } => {
                    require_path(path)?;
                    if contents.len() < 2 {
                        return Err("offline_edit 至少需要两个客户端的离线修改".into());
                    }
                    for (client, content) in contents {
                        known(&clients, client)?;
                        require_content(path, content)?;
                    }
                }
                Block::Rename {
                    renamer,
                    placeholder_client,
                    from,
                    to,
                    ..
                } => {
                    require_path(from)?;
                    require_path(to)?;
                    if from == to {
                        return Err("rename 的 from/to 不能相同".into());
                    }
                    for client in [renamer, placeholder_client] {
                        known(&clients, client)?;
                    }
                    if renamer == placeholder_client {
                        return Err("rename 的 renamer 与 placeholder_client 不能相同".into());
                    }
                }
                Block::DeleteModify {
                    path,
                    deleter,
                    modifier,
                    modified_content,
                    ..
                } => {
                    require_path(path)?;
                    for client in [deleter, modifier] {
                        known(&clients, client)?;
                    }
                    if deleter == modifier {
                        return Err("delete_modify 的 deleter 与 modifier 不能相同".into());
                    }
                    require_content(path, modified_content)?;
                }
                Block::LostResponse { put, delete, .. } => {
                    require_path(&put.path)?;
                    require_path(&delete.path)?;
                    if put.path == delete.path {
                        return Err("lost_response 的 put 与 delete 必须使用不同路径".into());
                    }
                    for client in [&put.client, &delete.client] {
                        known(&clients, client)?;
                    }
                    require_content(&put.path, &put.initial_content)?;
                    require_content(&put.path, &put.content)?;
                    require_content(&delete.path, &delete.content)?;
                }
            }
        }
        Ok(())
    }

    /// 场景声明里的全部输入内容（含服务端种子）：`(说明, 内容)`。
    #[must_use]
    pub fn inputs(&self) -> Vec<(String, String)> {
        let mut inputs = Vec::new();
        for file in &self.server_seed {
            inputs.push((format!("server:{}", file.path), file.content.clone()));
        }
        for block in &self.blocks {
            match block {
                Block::Import { id, local, .. } => {
                    for (client, files) in local {
                        for file in files {
                            inputs.push((
                                format!("{id}:{client}:{}", file.path),
                                file.content.clone(),
                            ));
                        }
                    }
                }
                Block::OfflineEdit {
                    id, path, contents, ..
                } => {
                    for (client, content) in contents {
                        inputs.push((format!("{id}:{client}:{path}"), content.clone()));
                    }
                }
                Block::Rename { .. } => {}
                Block::DeleteModify {
                    id,
                    path,
                    modified_content,
                    ..
                } => {
                    inputs.push((format!("{id}:modifier:{path}"), modified_content.clone()));
                }
                Block::LostResponse {
                    id, put, delete, ..
                } => {
                    inputs.push((
                        format!("{id}:put-initial:{}", put.path),
                        put.initial_content.clone(),
                    ));
                    inputs.push((format!("{id}:put-final:{}", put.path), put.content.clone()));
                    inputs.push((
                        format!("{id}:delete:{}", delete.path),
                        delete.content.clone(),
                    ));
                }
            }
        }
        inputs
    }
}

fn known(clients: &BTreeSet<&str>, client: &str) -> Result<(), String> {
    if clients.contains(client) {
        Ok(())
    } else {
        Err(format!("引用了未声明的客户端：{client}"))
    }
}

fn unique_paths(label: &str, files: &[FileSpec]) -> Result<(), String> {
    let mut paths = BTreeSet::new();
    for file in files {
        require_path(&file.path)?;
        if !paths.insert(file.path.as_str()) {
            return Err(format!("{label} 的路径重复：{}", file.path));
        }
    }
    Ok(())
}

fn require_token(what: &str, value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 100
        || !value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(format!("{what} 只能是小写字母、数字与 `-`：{value}"));
    }
    Ok(())
}

fn require_content(path: &str, content: &str) -> Result<(), String> {
    if content.is_empty() {
        return Err(format!("{path} 的内容不能为空"));
    }
    if content.len() > 64 * 1024 {
        return Err(format!("{path} 的内容超过 64 KiB 上限"));
    }
    Ok(())
}

fn require_path(path: &str) -> Result<(), String> {
    if path.is_empty() || path.len() > 200 {
        return Err(format!("路径长度越界：{path:?}"));
    }
    if path.starts_with('/') || path.contains('\\') || path.contains(':') {
        return Err(format!("路径必须是相对的 `/` 分隔路径：{path}"));
    }
    for component in path.split('/') {
        if component.is_empty() || component == "." || component == ".." {
            return Err(format!("路径含空、`.` 或 `..` 分量：{path}"));
        }
        if component.chars().any(char::is_control) {
            return Err(format!("路径含控制字符：{path}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scenario_path() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../scenarios/three-clients.json")
    }

    #[test]
    fn repository_scenario_is_valid_and_covers_the_card() {
        let scenario = Scenario::load(&scenario_path()).expect("仓库场景必须有效");
        assert_eq!(scenario.id, "s074-three-clients");
        assert_eq!(scenario.clients.len(), 3);
        assert_eq!(
            scenario
                .clients
                .iter()
                .filter(|client| client.mode == Mode::OnDemand)
                .count(),
            1,
            "rename 场景需要一个仅占位的按需端"
        );
        for requirement in ["T01", "T02", "T03", "T09", "T11"] {
            assert!(
                scenario.requirements.iter().any(|r| r == requirement),
                "缺少验收编号 {requirement}"
            );
        }
        let kinds: Vec<&str> = scenario.blocks.iter().map(Block::id).collect();
        assert_eq!(
            kinds,
            [
                "T01-import",
                "T02-offline-edit",
                "T09-rename",
                "T11-delete-modify",
                "T03-lost-response"
            ]
        );
        // 每份输入内容都要能被单独核对：除了刻意同文的 shared-import，其余内容必须两两不同。
        let inputs = scenario.inputs();
        assert!(inputs.len() >= 15, "输入内容数量异常：{}", inputs.len());
        let shared: BTreeSet<&str> = scenario
            .server_seed
            .iter()
            .filter(|file| file.path == "shared-import.txt")
            .map(|file| file.content.as_str())
            .collect();
        assert_eq!(shared.len(), 1);
        let shared = shared.into_iter().next().unwrap();
        let mut seen = BTreeSet::new();
        for (label, content) in &inputs {
            if content == shared {
                continue;
            }
            assert!(
                seen.insert(content.clone()),
                "输入内容重复，无法逐份核对：{label}"
            );
        }
        assert!(
            seen.len() + 1 == inputs.len() - 3,
            "只有 shared-import 允许三端同文：seen={} inputs={}",
            seen.len(),
            inputs.len()
        );
        for block in &scenario.blocks {
            if let Block::Import { local, .. } = block {
                for files in local.values() {
                    assert!(
                        files
                            .iter()
                            .any(|file| file.path == "shared-import.txt" && file.content == shared),
                        "每个客户端都应以同文导入 shared-import.txt"
                    );
                }
            }
        }
    }

    #[test]
    fn rejects_unknown_fields_and_bad_references() {
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(scenario_path()).unwrap()).unwrap();
        value["blocks"][0]["local"]["nobody"] = serde_json::json!([]);
        assert!(
            Scenario::parse(&serde_json::to_vec(&value).unwrap())
                .unwrap_err()
                .contains("未声明的客户端")
        );
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(scenario_path()).unwrap()).unwrap();
        value["blocks"][2]["kind"] = serde_json::json!("rename_typo");
        assert!(Scenario::parse(&serde_json::to_vec(&value).unwrap()).is_err());
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(scenario_path()).unwrap()).unwrap();
        value["schema"] = serde_json::json!(2);
        assert!(Scenario::parse(&serde_json::to_vec(&value).unwrap()).is_err());
    }

    #[test]
    fn rejects_unsafe_paths_and_empty_content() {
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(scenario_path()).unwrap()).unwrap();
        value["server_seed"][0]["path"] = serde_json::json!("../escape.txt");
        assert!(
            Scenario::parse(&serde_json::to_vec(&value).unwrap())
                .unwrap_err()
                .contains("`..`")
        );
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(scenario_path()).unwrap()).unwrap();
        value["server_seed"][0]["content"] = serde_json::json!("");
        assert!(Scenario::parse(&serde_json::to_vec(&value).unwrap()).is_err());
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(scenario_path()).unwrap()).unwrap();
        value["clients"][0]["id"] = serde_json::json!("Alpha");
        assert!(Scenario::parse(&serde_json::to_vec(&value).unwrap()).is_err());
    }
}
