//! Download the public Windows ZIP from this project's latest stable GitHub release.
//! No server credentials, local state, program replacement or process control.
#![forbid(unsafe_code)]
use reqwest::{Client, StatusCode};
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::time::Duration;

const REPO: &str = "https://github.com/xingjianxu/vaultsyncd";
const LATEST: &str = "https://api.github.com/repos/xingjianxu/vaultsyncd/releases/latest";
const MAX_METADATA: u64 = 1024 * 1024;
const MAX_PACKAGE: u64 = 1024 * 1024 * 1024;

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}
#[derive(Deserialize)]
struct Asset {
    name: String,
    state: String,
    size: u64,
    digest: Option<String>,
    browser_download_url: String,
}

/// A strictly validated stable Windows x64 ZIP; fields cannot be supplied by the GUI.
pub struct ReleaseUpdate {
    version: String,
    asset: Asset,
}
impl ReleaseUpdate {
    /// Stable semantic version without the tag's `v` prefix.
    pub fn version(&self) -> &str {
        &self.version
    }
    /// Safe basename from the fixed release package naming convention.
    pub fn filename(&self) -> &str {
        &self.asset.name
    }
}

/// Independent HTTPS client with system certificate roots and bounded requests.
pub struct GithubUpdates(Client);
impl GithubUpdates {
    /// Create a public release client without any application session or credentials.
    pub fn new() -> Result<Self, String> {
        Client::builder()
            .https_only(true)
            .user_agent(concat!("vaultsyncd/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(30 * 60))
            .redirect(reqwest::redirect::Policy::custom(|attempt| {
                let url = attempt.url();
                let host = url.host_str().unwrap_or_default();
                if attempt.previous().len() >= 5 {
                    attempt.error("更新下载重定向次数过多")
                } else if url.scheme() == "https"
                    && url.port_or_known_default() == Some(443)
                    && url.username().is_empty()
                    && url.password().is_none()
                    && matches!(
                        host,
                        "github.com"
                            | "api.github.com"
                            | "release-assets.githubusercontent.com"
                            | "objects.githubusercontent.com"
                    )
                {
                    attempt.follow()
                } else {
                    attempt.error("更新下载重定向地址不可信")
                }
            }))
            .build()
            .map(Self)
            .map_err(|_| "无法初始化更新连接".into())
    }

    /// Check GitHub's latest stable release. Equal/older versions never downgrade.
    pub async fn check(&self, current: &str) -> Result<Option<ReleaseUpdate>, String> {
        let mut response = self
            .0
            .get(LATEST)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .timeout(Duration::from_secs(45))
            .send()
            .await
            .map_err(|_| "无法连接 GitHub，请检查网络后重试")?;
        if response.status() == StatusCode::NOT_FOUND {
            return Err("GitHub 尚无可访问的正式 Release，或仓库不可访问".into());
        }
        status(response.status())?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| "读取版本信息失败，请重试")?
        {
            if bytes.len() as u64 + chunk.len() as u64 > MAX_METADATA {
                return Err("GitHub 版本信息超过大小限制".into());
            }
            bytes.extend_from_slice(&chunk);
        }
        candidate(
            serde_json::from_slice(&bytes).map_err(|_| "GitHub 版本信息无效")?,
            current,
        )
    }

    /// Stream untrusted bytes to a temporary destination. Only successful return means
    /// the entire package matches the release's length and SHA-256. On any error the
    /// caller must discard partial output and never open it as a completed download.
    pub async fn download(
        &self,
        update: &ReleaseUpdate,
        sink: impl FnMut(&[u8], u64, u64) -> Result<(), String>,
    ) -> Result<(), String> {
        let response = self
            .0
            .get(&update.asset.browser_download_url)
            .send()
            .await
            .map_err(|_| "无法下载更新，请检查网络后重试")?;
        receive(response, &update.asset, sink).await
    }
}

fn status(code: StatusCode) -> Result<(), String> {
    match code {
        StatusCode::OK => Ok(()),
        StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS => {
            Err("GitHub 访问受限或请求过于频繁，请稍后重试".into())
        }
        _ => Err(format!(
            "GitHub 请求失败（HTTP {}），请稍后重试",
            code.as_u16()
        )),
    }
}

fn candidate(release: Release, current: &str) -> Result<Option<ReleaseUpdate>, String> {
    let version = release
        .tag_name
        .strip_prefix('v')
        .unwrap_or(&release.tag_name);
    let parsed = Version::parse(version).map_err(|_| "Release 版本号无效")?;
    if release.draft || release.prerelease || !parsed.pre.is_empty() || !parsed.build.is_empty() {
        return Err("自动更新仅接受已发布的正式版本".into());
    }
    if parsed <= Version::parse(current).map_err(|_| "当前客户端版本无效")? {
        return Ok(None);
    }
    let name = format!("vaultsyncd-{version}-x64-unsigned.zip");
    let expected_url = format!("{REPO}/releases/download/{}/{name}", release.tag_name);
    let mut matching = release.assets.into_iter().filter(|a| a.name == name);
    let asset = matching.next().ok_or("Release 缺少 Windows x64 更新包")?;
    if matching.next().is_some()
        || asset.state != "uploaded"
        || asset.size == 0
        || asset.size > MAX_PACKAGE
        || asset.browser_download_url != expected_url
    {
        return Err("Release 更新包来源、大小或状态无效".into());
    }
    let hash = asset
        .digest
        .as_deref()
        .and_then(|s| s.strip_prefix("sha256:"))
        .filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()));
    if hash.is_none() {
        return Err("Release 更新包缺少有效 SHA-256 校验，无法安全下载".into());
    }
    Ok(Some(ReleaseUpdate {
        version: version.to_owned(),
        asset,
    }))
}

async fn receive(
    mut response: reqwest::Response,
    asset: &Asset,
    mut sink: impl FnMut(&[u8], u64, u64) -> Result<(), String>,
) -> Result<(), String> {
    status(response.status())?;
    if response.content_length().is_some_and(|n| n != asset.size) {
        return Err("更新包大小与 Release 不符".into());
    }
    let mut hash = Sha256::new();
    let mut received = 0;
    while let Some(chunk) = response.chunk().await.map_err(|_| "更新下载中断，请重试")? {
        received += chunk.len() as u64;
        if received > asset.size {
            return Err("更新包超过声明大小".into());
        }
        hash.update(&chunk);
        sink(&chunk, received, asset.size)?;
    }
    let actual = format!("sha256:{:x}", hash.finalize());
    if received != asset.size
        || !asset
            .digest
            .as_ref()
            .is_some_and(|s| s.eq_ignore_ascii_case(&actual))
    {
        return Err("更新包 SHA-256 或大小校验失败，请重新下载".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
