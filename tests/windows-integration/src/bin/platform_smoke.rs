//! 最小 Windows 平台冒烟：在客体上打印真实的 OS 版本、系统架构，以及用
//! `version.dll` 读出的 `kernel32.dll` 文件版本。
//!
//! 该程序在 NixOS 宿主交叉编译、传入 Windows 客体原生执行
//! （`docs/operations/windows-lab.md` §12）。退出码 0 表示所有 Win32 调用成功且
//! 架构为 x64；1 表示任一调用失败。输出只用 ASCII：客体控制台代码页实测为 437。

#![allow(unsafe_code)]
// 上面一行是本 bin 的唯一 lint 例外：冒烟程序必须直接声明并调用 Win32/CFAPI 函数。
// 其余 lint（missing_docs、unused、clippy all 等）仍继承 workspace 设置。

#[cfg(windows)]
mod platform {
    use std::ffi::{OsStr, c_void};
    use std::os::windows::ffi::OsStrExt;

    /// `PROCESSOR_ARCHITECTURE_AMD64`；首发只支持 Windows 11 x64。
    const PROCESSOR_ARCHITECTURE_AMD64: u16 = 9;

    /// `SYSTEM_INFO`；只需要 `wProcessorArchitecture`，但按原布局完整声明。
    #[repr(C)]
    struct SystemInfo {
        processor_architecture: u16,
        reserved: u16,
        page_size: u32,
        minimum_application_address: *mut c_void,
        maximum_application_address: *mut c_void,
        active_processor_mask: usize,
        number_of_processors: u32,
        processor_type: u32,
        allocation_granularity: u32,
        processor_level: u16,
        processor_revision: u16,
    }

    /// `RTL_OSVERSIONINFOW`：`RtlGetVersion` 报告的内核版本。
    #[repr(C)]
    struct OsVersionInfoW {
        os_version_info_size: u32,
        major_version: u32,
        minor_version: u32,
        build_number: u32,
        platform_id: u32,
        csd_version: [u16; 128],
    }

    /// `VS_FIXEDFILEINFO`：与 `VerQueryValueW(L"\\")` 返回的根块一致。
    #[repr(C)]
    struct VsFixedFileInfo {
        signature: u32,
        struct_version: u32,
        file_version_ms: u32,
        file_version_ls: u32,
        product_version_ms: u32,
        product_version_ls: u32,
        file_flags_mask: u32,
        file_flags: u32,
        file_os: u32,
        file_type: u32,
        file_subtype: u32,
        file_date_ms: u32,
        file_date_ls: u32,
    }

    /// `VS_FFI_SIGNATURE`：`VS_FIXEDFILEINFO.dwSignature` 的固定值。
    const VS_FFI_SIGNATURE: u32 = 0xFEEF_04BD;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetSystemInfo(system_info: *mut SystemInfo);
    }

    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn RtlGetVersion(version_info: *mut OsVersionInfoW) -> i32;
    }

    #[link(name = "version")]
    unsafe extern "system" {
        fn GetFileVersionInfoSizeW(file_name: *const u16, handle: *mut u32) -> u32;
        fn GetFileVersionInfoW(
            file_name: *const u16,
            handle: u32,
            length: u32,
            data: *mut c_void,
        ) -> i32;
        fn VerQueryValueW(
            block: *const c_void,
            sub_block: *const u16,
            buffer: *mut *mut c_void,
            length: *mut u32,
        ) -> i32;
    }

    /// 转成以 NUL 结尾的宽字符串。
    fn wide(text: &str) -> Vec<u16> {
        OsStr::new(text).encode_wide().chain(Some(0)).collect()
    }

    fn last_error() -> String {
        std::io::Error::last_os_error().to_string()
    }

    /// 取 `SYSTEM_INFO` 报告的处理器架构。
    fn processor_architecture() -> u16 {
        let mut info = std::mem::MaybeUninit::<SystemInfo>::zeroed();
        // SAFETY: GetSystemInfo 只写满整个 SYSTEM_INFO；指针指向正确大小的栈上值。
        let system_info = unsafe {
            GetSystemInfo(info.as_mut_ptr());
            info.assume_init()
        };
        system_info.processor_architecture
    }

    /// 取 `RtlGetVersion` 报告的内核版本，格式 `major.minor (build N)`。
    fn kernel_version() -> Result<String, String> {
        let mut info = std::mem::MaybeUninit::<OsVersionInfoW>::zeroed();
        let version = unsafe {
            let version = info.as_mut_ptr();
            (*version).os_version_info_size = size_of::<OsVersionInfoW>() as u32;
            let status = RtlGetVersion(version);
            if status != 0 {
                return Err(format!("RtlGetVersion returned NTSTATUS 0x{status:08X}"));
            }
            info.assume_init()
        };
        Ok(format!(
            "{}.{} (build {})",
            version.major_version, version.minor_version, version.build_number
        ))
    }

    /// 用 `version.dll` 读取指定文件的固定版本信息，格式 `a.b.c.d`。
    fn file_version(path: &str) -> Result<String, String> {
        let file = wide(path);
        let mut handle: u32 = 0;
        // SAFETY: 传入以 NUL 结尾的宽字符串指针与可写的 handle 变量。
        let size = unsafe { GetFileVersionInfoSizeW(file.as_ptr(), &mut handle) };
        if size == 0 {
            return Err(format!("GetFileVersionInfoSizeW({path}): {}", last_error()));
        }
        let mut data = vec![0u8; size as usize];
        // SAFETY: 缓冲区长度等于上一步返回的 size，函数按该长度写入。
        if unsafe { GetFileVersionInfoW(file.as_ptr(), handle, size, data.as_mut_ptr().cast()) }
            == 0
        {
            return Err(format!("GetFileVersionInfoW({path}): {}", last_error()));
        }
        let vault = wide("\\");
        let mut block: *mut c_void = std::ptr::null_mut();
        let mut block_length: u32 = 0;
        // SAFETY: 查询根块只在 data 内取指针，block_length 校验后才解引用。
        if unsafe {
            VerQueryValueW(
                data.as_ptr().cast(),
                vault.as_ptr(),
                &mut block,
                &mut block_length,
            )
        } == 0
        {
            return Err(format!("VerQueryValueW({path}, \\): {}", last_error()));
        }
        if block.is_null() || block_length < size_of::<VsFixedFileInfo>() as u32 {
            return Err(format!(
                "VerQueryValueW({path}) returned {block_length} bytes, expected VS_FIXEDFILEINFO"
            ));
        }
        // SAFETY: 上面已确认非空且长度覆盖 VS_FIXEDFILEINFO。
        let fixed = unsafe { &*(block.cast::<VsFixedFileInfo>()) };
        if fixed.signature != VS_FFI_SIGNATURE {
            return Err(format!(
                "VS_FIXEDFILEINFO signature 0x{:08X} is not 0x{VS_FFI_SIGNATURE:08X}",
                fixed.signature
            ));
        }
        let (ms, ls) = (fixed.file_version_ms, fixed.file_version_ls);
        Ok(format!(
            "{}.{}.{}.{}",
            ms >> 16,
            ms & 0xFFFF,
            ls >> 16,
            ls & 0xFFFF
        ))
    }

    /// 依次执行三项检查；任一失败即返回错误文本。
    pub fn run() -> Result<(), String> {
        println!(
            "build_target: {}-{}",
            std::env::consts::ARCH,
            std::env::consts::OS
        );

        let architecture = processor_architecture();
        println!("GetSystemInfo.wProcessorArchitecture: {architecture}");
        if architecture != PROCESSOR_ARCHITECTURE_AMD64 {
            return Err(format!(
                "expected PROCESSOR_ARCHITECTURE_AMD64 ({PROCESSOR_ARCHITECTURE_AMD64}), got {architecture}"
            ));
        }

        let version = kernel_version()?;
        println!("RtlGetVersion: {version}");

        let version_dll = file_version("C:\\Windows\\System32\\version.dll")?;
        println!("version.dll file_version: {version_dll}");
        let kernel32_dll = file_version("C:\\Windows\\System32\\kernel32.dll")?;
        println!("kernel32.dll file_version: {kernel32_dll}");

        Ok(())
    }
}

fn main() -> std::process::ExitCode {
    #[cfg(windows)]
    {
        match platform::run() {
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
            "platform-smoke: build for x86_64-pc-windows-msvc on the NixOS host and run it on a Windows guest"
        );
        std::process::ExitCode::FAILURE
    }
}
