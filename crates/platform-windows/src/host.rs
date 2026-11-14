//! 当前运行主机的展示属性；不用于认证或设备身份。
use std::io;

#[link(name = "kernel32")]
unsafe extern "system" {
    fn GetComputerNameExW(format: i32, buffer: *mut u16, length: *mut u32) -> i32;
}

#[repr(C)]
struct Version {
    size: u32,
    major: u32,
    minor: u32,
    build: u32,
    platform: u32,
    service_pack: [u16; 128],
}

#[link(name = "ntdll")]
unsafe extern "system" {
    fn RtlGetVersion(version: *mut Version) -> i32;
}

/// 从 Windows 读取 DNS 主机名，不使用环境变量或固定应用名称。
pub fn hostname() -> io::Result<String> {
    let mut buffer = [0u16; 256];
    let mut length = buffer.len() as u32;
    // SAFETY: 缓冲区大小与 length 一致，调用期间保持有效。
    if unsafe { GetComputerNameExW(1, buffer.as_mut_ptr(), &mut length) } == 0 {
        return Err(io::Error::last_os_error());
    }
    String::from_utf16(&buffer[..length as usize])
        .map_err(|cause| io::Error::new(io::ErrorKind::InvalidData, cause))
}

/// 读取真实 NT 内核版本；不受应用清单的版本兼容声明影响。
pub fn operating_system() -> io::Result<String> {
    let mut version = Version {
        size: std::mem::size_of::<Version>() as u32,
        major: 0,
        minor: 0,
        build: 0,
        platform: 0,
        service_pack: [0; 128],
    };
    // SAFETY: repr(C) 与 OSVERSIONINFOW 一致，size 包含整个可写结构。
    let status = unsafe { RtlGetVersion(&mut version) };
    if status < 0 {
        return Err(io::Error::other(format!("RtlGetVersion: {status:#x}")));
    }
    let name = if version.major == 10 && version.build >= 22000 {
        "Windows 11"
    } else {
        "Windows"
    };
    Ok(format!(
        "{name} ({}.{}.{}) x64",
        version.major, version.minor, version.build
    ))
}

#[cfg(test)]
mod tests {
    #[test]
    fn reads_native_host_properties() {
        let hostname = super::hostname().unwrap();
        let os = super::operating_system().unwrap();
        assert!(!hostname.is_empty());
        assert!(hostname.eq_ignore_ascii_case(&std::env::var("COMPUTERNAME").unwrap()));
        assert!(os.starts_with("Windows 11 (10.0."));
        println!("hostname={hostname}; operating_system={os}");
    }
}
