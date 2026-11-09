//! Target-aware dependency and protocol consumer gates (S092).
use crate::{
    EXIT_FAILURE, EXIT_OK, capture, describe_exit_code, generate, reject_arguments, workspace_root,
};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

const TARGETS: &[&str] = &["x86_64-unknown-linux-gnu", "x86_64-pc-windows-msvc"];
#[derive(Deserialize)]
struct Metadata {
    packages: Vec<Package>,
    resolve: Resolve,
}
#[derive(Deserialize)]
struct Package {
    id: String,
    name: String,
}
#[derive(Deserialize)]
struct Resolve {
    nodes: Vec<Node>,
}
#[derive(Deserialize)]
struct Node {
    id: String,
    deps: Vec<Dependency>,
}
#[derive(Deserialize)]
struct Dependency {
    pkg: String,
    dep_kinds: Vec<Kind>,
}
#[derive(Deserialize)]
struct Kind {
    kind: Option<String>,
}

fn database(name: &str) -> bool {
    [
        "sqlx",
        "sqlite",
        "rusqlite",
        "libsqlite",
        "duckdb",
        "diesel",
        "better-sqlite",
        "@tauri-apps/plugin-sql",
    ]
    .iter()
    .any(|prefix| name.starts_with(prefix))
}
fn tauri(name: &str) -> bool {
    name == "tauri" || name.starts_with("tauri-") || name.starts_with("@tauri-apps/")
}
fn fuse(name: &str) -> bool {
    ["fuse", "fuser", "polyfuse"]
        .iter()
        .any(|prefix| name.starts_with(prefix))
}
fn forbidden(consumer: &str, dependency: &str, target: &str) -> bool {
    match consumer {
        "vaultsyncd-server"
        | "vaultsyncd-server-core"
        | "vaultsyncd-server-store"
        | "vaultsyncd-admin" => {
            tauri(dependency)
                || fuse(dependency)
                || [
                    "vaultsyncd-agent",
                    "vaultsyncd-agent-core",
                    "vaultsyncd-client-store",
                    "vaultsyncd-platform-windows",
                    "vaultsyncd-cli",
                    "vaultsyncd-desktop",
                ]
                .contains(&dependency)
        }
        "vaultsyncd-domain" | "vaultsyncd-planner" => {
            database(dependency)
                || tauri(dependency)
                || fuse(dependency)
                || ["tokio", "reqwest", "hyper", "axum", "windows"].contains(&dependency)
                || dependency.starts_with("vaultsyncd-platform-")
        }
        "vaultsyncd-protocol" | "vaultsyncd-local-ipc" => {
            database(dependency)
                || tauri(dependency)
                || dependency.starts_with("vaultsyncd-platform-")
                || ["vaultsyncd-server-core", "vaultsyncd-agent-core"].contains(&dependency)
        }
        "vaultsyncd-agent-core" => {
            tauri(dependency)
                || fuse(dependency)
                || [
                    "vaultsyncd-server-core",
                    "vaultsyncd-server-store",
                    "vaultsyncd-platform-linux-server",
                ]
                .contains(&dependency)
        }
        "vaultsyncd-agent" => dependency == "vaultsyncd-platform-linux-server",
        "vaultsyncd-release-client" => {
            database(dependency) || tauri(dependency) || dependency.starts_with("vaultsyncd-")
        }
        "vaultsyncd-desktop" | "vaultsyncd-cli" => {
            database(dependency)
                || [
                    "vaultsyncd-client-store",
                    "vaultsyncd-server-store",
                    "vaultsyncd-agent-core",
                    "vaultsyncd-server-core",
                    "vaultsyncd-planner",
                    "vaultsyncd-http-client",
                    "vaultsyncd-protocol",
                ]
                .contains(&dependency)
                || (target.ends_with("linux-gnu") && dependency == "tauri")
        }
        "@vaultsyncd/ui" => {
            tauri(dependency) || dependency.starts_with("@vaultsyncd/") || database(dependency)
        }
        "@vaultsyncd/web" => {
            tauri(dependency) || dependency == "@vaultsyncd/local-ipc-types" || database(dependency)
        }
        "@vaultsyncd/desktop" => dependency == "@vaultsyncd/api-client" || database(dependency),
        "@vaultsyncd/api-client" => {
            tauri(dependency)
                || ["@vaultsyncd/ui", "@vaultsyncd/local-ipc-types"].contains(&dependency)
                || database(dependency)
        }
        "@vaultsyncd/local-ipc-types" => {
            tauri(dependency)
                || ["@vaultsyncd/ui", "@vaultsyncd/api-client"].contains(&dependency)
                || database(dependency)
        }
        _ => false,
    }
}
fn walk(
    consumer: &str,
    current: &str,
    graph: &BTreeMap<String, Vec<String>>,
    target: &str,
    seen: &mut BTreeSet<String>,
    path: &mut Vec<String>,
) -> Result<(), String> {
    if !seen.insert(current.to_owned()) {
        return Ok(());
    }
    for dependency in graph.get(current).into_iter().flatten() {
        path.push(dependency.clone());
        if forbidden(consumer, dependency, target) {
            return Err(format!("边界违规 [{target}]: {}", path.join(" -> ")));
        }
        walk(consumer, dependency, graph, target, seen, path)?;
        path.pop();
    }
    Ok(())
}
fn graph_check(graph: &BTreeMap<String, Vec<String>>, target: &str) -> Result<(), String> {
    for consumer in graph.keys() {
        walk(
            consumer,
            consumer,
            graph,
            target,
            &mut BTreeSet::new(),
            &mut vec![consumer.clone()],
        )?;
    }
    Ok(())
}
fn cargo_graph(root: &Path, target: &str) -> Result<(), String> {
    // Metadata only: all features activate optional edges without compiling cross-target applications.
    let output = capture(
        "cargo",
        &[
            "metadata",
            "--locked",
            "--format-version",
            "1",
            "--filter-platform",
            target,
            "--all-features",
        ],
        root,
    )?;
    if !output.status.success() {
        return Err(format!(
            "metadata [{target}] 失败: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let metadata: Metadata =
        serde_json::from_slice(&output.stdout).map_err(|e| format!("metadata JSON: {e}"))?;
    let names: BTreeMap<_, _> = metadata
        .packages
        .into_iter()
        .map(|p| (p.id, p.name))
        .collect();
    let mut graph = BTreeMap::new();
    for node in metadata.resolve.nodes {
        let deps = node
            .deps
            .into_iter()
            .filter(|dep| {
                dep.dep_kinds
                    .iter()
                    .any(|kind| kind.kind.as_deref() != Some("dev"))
            })
            .map(|dep| {
                names
                    .get(&dep.pkg)
                    .cloned()
                    .ok_or_else(|| format!("metadata 缺失依赖 {}", dep.pkg))
            })
            .collect::<Result<Vec<_>, _>>()?;
        graph
            .entry(names.get(&node.id).cloned().ok_or("metadata 缺失节点")?)
            .or_insert_with(Vec::new)
            .extend(deps);
    }
    graph_check(&graph, target)?;
    println!("  metadata [{target}] 依赖边界通过");
    Ok(())
}
fn read_json(path: &Path) -> Result<Value, String> {
    serde_json::from_slice(&fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?)
        .map_err(|e| format!("{}: {e}", path.display()))
}
fn pnpm_graph(root: &Path) -> Result<(), String> {
    let output = capture(
        "pnpm",
        &["list", "--recursive", "--depth", "-1", "--json"],
        root,
    )?;
    if !output.status.success() {
        return Err(format!(
            "pnpm 工作区查询失败: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let members: Vec<Value> =
        serde_json::from_slice(&output.stdout).map_err(|e| format!("pnpm JSON: {e}"))?;
    let mut graph = BTreeMap::new();
    for member in members {
        let path = member["path"].as_str().ok_or("pnpm 成员缺少路径")?;
        let manifest = read_json(&Path::new(path).join("package.json"))?;
        let name = manifest["name"]
            .as_str()
            .ok_or("pnpm 成员缺少包名")?
            .to_owned();
        let mut deps = BTreeSet::new();
        for section in [
            "dependencies",
            "devDependencies",
            "optionalDependencies",
            "peerDependencies",
        ] {
            if let Some(entries) = manifest[section].as_object() {
                for (name, version) in entries {
                    let real_name = version
                        .as_str()
                        .and_then(|s| s.strip_prefix("npm:"))
                        .and_then(|s| s.rsplit_once('@'))
                        .map_or(name.as_str(), |(name, _)| name);
                    deps.insert(real_name.to_owned());
                }
            }
        }
        graph.insert(name, deps.into_iter().collect());
    }
    for required in [
        "@vaultsyncd/ui",
        "@vaultsyncd/api-client",
        "@vaultsyncd/local-ipc-types",
        "@vaultsyncd/web",
        "@vaultsyncd/desktop",
    ] {
        if !graph.contains_key(required) {
            return Err(format!("pnpm 缺少协议消费者 {required}"));
        }
    }
    let installed = capture(
        "pnpm",
        &["list", "--recursive", "--depth", "Infinity", "--json"],
        root,
    )?;
    if !installed.status.success() {
        return Err("pnpm 已安装依赖图读取失败".into());
    }
    let trees: Vec<Value> = serde_json::from_slice(&installed.stdout).map_err(|e| e.to_string())?;
    for tree in trees {
        let name = tree["name"].as_str().ok_or("pnpm 依赖树缺少包名")?;
        installed_edges(name, &tree, &mut graph);
    }
    graph_check(&graph, "pnpm")?;
    println!("  pnpm 声明依赖与消费者边界通过");
    Ok(())
}
// Include external transitive dependencies and aliases, not only workspace manifests.
fn installed_edges(name: &str, tree: &Value, graph: &mut BTreeMap<String, Vec<String>>) {
    for section in ["dependencies", "devDependencies", "optionalDependencies"] {
        if let Some(deps) = tree[section].as_object() {
            for (key, node) in deps {
                let child = node["from"].as_str().unwrap_or(key);
                graph
                    .entry(name.to_owned())
                    .or_default()
                    .push(child.to_owned());
                installed_edges(child, node, graph);
            }
        }
    }
}
fn source_check(path: &Path) -> Result<(), String> {
    for item in fs::read_dir(path).map_err(|e| format!("{}: {e}", path.display()))? {
        let path = item.map_err(|e| e.to_string())?.path();
        if path.is_dir() {
            source_check(&path)?;
            continue;
        }
        if !matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("rs" | "ts" | "tsx" | "js" | "mjs")
        ) {
            continue;
        }
        let content = fs::read_to_string(&path).map_err(|e| e.to_string())?;
        for (index, line) in content.lines().enumerate() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            if [
                "sqlx::",
                "rusqlite::",
                "sqlite3",
                "vaultsyncd_client_store::",
                "vaultsyncd_server_store::",
                "INSERT INTO ",
                "DELETE FROM ",
            ]
            .iter()
            .any(|token| line.contains(token))
            {
                return Err(format!(
                    "GUI/CLI 数据库访问违规 {}:{}",
                    path.display(),
                    index + 1
                ));
            }
        }
    }
    Ok(())
}
pub(crate) fn verify(root: &Path) -> Result<(), String> {
    crate::release::verify_version(root)?;
    generate::verify()?;
    for target in TARGETS {
        cargo_graph(root, target)?;
    }
    pnpm_graph(root)?;
    for directory in [
        "apps/cli/src",
        "apps/desktop/src-tauri/src",
        "apps/desktop/src",
        "apps/web/src",
        "packages/ui/src",
    ] {
        source_check(&root.join(directory))?;
    }
    Ok(())
}
pub(crate) fn run(rest: &[String]) -> u8 {
    if let Some(code) = reject_arguments("check-boundaries", rest) {
        return code;
    }
    let result = (|| {
        let root = workspace_root()?;
        verify(&root)?;
        for args in [
            vec!["--recursive", "run", "typecheck"],
            vec!["--recursive", "--if-present", "run", "test"],
        ] {
            let status = std::process::Command::new("pnpm")
                .args(args)
                .current_dir(&root)
                .status()
                .map_err(|e| e.to_string())?;
            if !status.success() {
                return Err(format!(
                    "pnpm 协议消费者检查失败: {}",
                    describe_exit_code(&status)
                ));
            }
        }
        let status = std::process::Command::new("cargo")
            .args([
                "test",
                "--locked",
                "-p",
                "vaultsyncd-protocol",
                "-p",
                "vaultsyncd-local-ipc",
            ])
            .current_dir(&root)
            .status()
            .map_err(|e| e.to_string())?;
        if !status.success() {
            return Err("Rust 协议消费者测试失败".into());
        }
        Ok::<_, String>(())
    })();
    match result {
        Ok(()) => {
            println!("构建依赖与协议消费者检查通过");
            EXIT_OK
        }
        Err(e) => {
            eprintln!("xtask: {e}");
            EXIT_FAILURE
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn policy_keeps_adapters_and_target_scopes() {
        assert!(!forbidden(
            "vaultsyncd-server-store",
            "vaultsyncd-server-core",
            TARGETS[0]
        ));
        assert!(!forbidden(
            "vaultsyncd-desktop",
            "tauri",
            "x86_64-pc-windows-msvc"
        ));
        assert!(forbidden("vaultsyncd-desktop", "tauri", TARGETS[0]));
        assert!(forbidden(
            "vaultsyncd-desktop",
            "vaultsyncd-http-client",
            TARGETS[1]
        ));
        assert!(!forbidden(
            "vaultsyncd-desktop",
            "vaultsyncd-release-client",
            TARGETS[1]
        ));
        assert!(forbidden(
            "vaultsyncd-release-client",
            "vaultsyncd-protocol",
            TARGETS[1]
        ));
        assert!(forbidden("vaultsyncd-release-client", "sqlx", TARGETS[1]));
        assert!(!forbidden(
            "vaultsyncd-release-client",
            "reqwest",
            TARGETS[1]
        ));
        assert!(!forbidden(
            "vaultsyncd-cli",
            "vaultsyncd-platform-windows",
            "x86_64-pc-windows-msvc"
        ));
        assert!(forbidden(
            "vaultsyncd-server",
            "vaultsyncd-client-store",
            TARGETS[0]
        ));
        assert!(forbidden("@vaultsyncd/ui", "@tauri-apps/api", "pnpm"));
    }
    #[test]
    fn indirect_edges_report_the_full_chain() {
        let graph = BTreeMap::from([
            ("vaultsyncd-server".into(), vec!["adapter".into()]),
            ("adapter".into(), vec!["fuser".into()]),
        ]);
        assert!(
            graph_check(&graph, TARGETS[0])
                .unwrap_err()
                .contains("vaultsyncd-server -> adapter -> fuser")
        );
    }
}
