//! CFAPI 链接与运行时解析冒烟：调用 `CfGetPlatformInfo`，确认宿主交叉编译能链接
//! Windows SDK 的 `cldapi.lib`、客体运行时能解析 `CfGetPlatformInfo` 导出。
//!
//! 该程序只读查询平台版本，不注册同步根、不改动客体状态；CFAPI 行为原型属 S010—
//! S012。宿主交叉编译、客体原生执行（`docs/operations/windows-lab.md` §12），
//! 退出码 0 表示 HRESULT 成功，1 表示调用失败。输出只用 ASCII。

#![allow(unsafe_code)]
// 上面一行是本 bin 的唯一 lint 例外：冒烟程序必须直接声明并调用 Win32/CFAPI 函数。
// 其余 lint（missing_docs、unused、clippy all 等）仍继承 workspace 设置。

#[cfg(windows)]
mod cfapi {
    /// `CF_PLATFORM_INFO`（Windows SDK `cfapi.h`）：三个 DWORD 的平台版本号。
    #[repr(C)]
    struct CfPlatformInfo {
        build_number: u32,
        revision_number: u32,
        integration_number: u32,
    }

    #[link(name = "cldapi")]
    unsafe extern "system" {
        /// `STDAPI CfGetPlatformInfo(_Out_ CF_PLATFORM_INFO *)`。
        fn CfGetPlatformInfo(platform_version: *mut CfPlatformInfo) -> i32;
    }

    /// HRESULT 成功判定（`SUCCEEDED`）。
    fn succeeded(hr: i32) -> bool {
        hr >= 0
    }

    /// 调用一次 `CfGetPlatformInfo` 并打印原始版本字段。
    pub fn run() -> Result<(), String> {
        let mut info = std::mem::MaybeUninit::<CfPlatformInfo>::zeroed();
        let (hr, info) = unsafe {
            let info = info.as_mut_ptr();
            let hr = CfGetPlatformInfo(info);
            (hr, info.read())
        };
        println!("CfGetPlatformInfo HRESULT: 0x{hr:08X}");
        let build = info.build_number;
        let revision = info.revision_number;
        let integration = info.integration_number;
        println!("CF_PLATFORM_INFO: build={build} revision={revision} integration={integration}");
        if !succeeded(hr) {
            return Err(format!("CfGetPlatformInfo failed with HRESULT 0x{hr:08X}"));
        }
        Ok(())
    }
}

fn main() -> std::process::ExitCode {
    #[cfg(windows)]
    {
        println!("cfapi_link_smoke: calling CfGetPlatformInfo");
        match cfapi::run() {
            Ok(()) => {
                println!("result: OK");
                std::process::ExitCode::SUCCESS
            }
            Err(error) => {
                println!("result: FAILED");
                eprintln!("error: {error}");
                std::process::ExitCode::FAILURE
            }
        }
    }
    #[cfg(not(windows))]
    {
        eprintln!(
            "cfapi-link-smoke: build for x86_64-pc-windows-msvc on the NixOS host and run it on a Windows guest"
        );
        std::process::ExitCode::FAILURE
    }
}
