/// 平台原生剪贴板文件列表读取和写入
/// Windows: 读取/写入 CF_HDROP 格式
/// Linux: 通过检测 text/uri-list 格式的文本内容解析文件路径，写入时同时设置多种 MIME 类型
use log::{debug, error, info};
#[cfg(target_os = "linux")]
use log::warn;
use std::path::PathBuf;

/// 尝试从系统剪贴板读取文件列表（文件管理器复制的文件）
/// 返回 Some(Vec<PathBuf>) 表示成功读取到文件列表
/// 返回 None 表示剪贴板中没有文件列表
pub fn get_clipboard_file_list() -> Option<Vec<PathBuf>> {
    #[cfg(target_os = "windows")]
    {
        get_clipboard_file_list_windows()
    }
    #[cfg(target_os = "linux")]
    {
        get_clipboard_file_list_linux()
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        None
    }
}

#[cfg(target_os = "windows")]
fn get_clipboard_file_list_windows() -> Option<Vec<PathBuf>> {
    use windows::Win32::Foundation::{HWND, HGLOBAL};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    };
    use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
    use windows::Win32::System::Ole::CF_HDROP;

    unsafe {
        // 检查是否有 CF_HDROP 格式
        if IsClipboardFormatAvailable(CF_HDROP.0 as u32).is_err() {
            return None;
        }

        if OpenClipboard(HWND::default()).is_err() {
            error!("无法打开剪贴板");
            return None;
        }

        let result = (|| -> Option<Vec<PathBuf>> {
            let handle = match GetClipboardData(CF_HDROP.0 as u32) {
                Ok(h) => h,
                Err(e) => {
                    error!("获取 CF_HDROP 数据失败: {}", e);
                    return None;
                }
            };

            let hmem: HGLOBAL = std::mem::transmute(handle);
            let ptr = GlobalLock(hmem);
            if ptr.is_null() {
                error!("GlobalLock 失败");
                return None;
            }

            let drop_files = ptr as *const DROPFILES;
            let wide = (*drop_files).fWide.as_bool();
            let offset = (*drop_files).pFiles as usize;
            let base = (ptr as *const u8).add(offset);

            let mut files = Vec::new();

            if wide {
                // UTF-16 编码的文件路径列表，以双 null 结尾
                let mut pos = base as *const u16;
                loop {
                    if *pos == 0 {
                        break;
                    }
                    let start = pos;
                    let mut len = 0usize;
                    while *pos != 0 {
                        pos = pos.add(1);
                        len += 1;
                    }
                    let slice = std::slice::from_raw_parts(start, len);
                    let path_str = String::from_utf16_lossy(slice);
                    debug!("CF_HDROP 文件: {}", path_str);
                    files.push(PathBuf::from(path_str));
                    pos = pos.add(1); // 跳过 null 终止符
                }
            } else {
                // ANSI 编码的文件路径列表
                let mut pos = base;
                loop {
                    if *pos == 0 {
                        break;
                    }
                    let start = pos;
                    let mut len = 0usize;
                    while *pos != 0 {
                        pos = pos.add(1);
                        len += 1;
                    }
                    let slice = std::slice::from_raw_parts(start, len);
                    let path_str = String::from_utf8_lossy(slice).to_string();
                    debug!("CF_HDROP 文件 (ANSI): {}", path_str);
                    files.push(PathBuf::from(path_str));
                    pos = pos.add(1);
                }
            }

            let _ = GlobalUnlock(hmem);

            if files.is_empty() {
                None
            } else {
                Some(files)
            }
        })();

        let _ = CloseClipboard();
        result
    }
}

/// Windows DROPFILES 结构体
#[cfg(target_os = "windows")]
#[repr(C)]
#[allow(non_snake_case)]
struct DROPFILES {
    pFiles: u32,
    pt_x: i32,
    pt_y: i32,
    fNC: windows::Win32::Foundation::BOOL,
    fWide: windows::Win32::Foundation::BOOL,
}

#[cfg(target_os = "linux")]
fn get_clipboard_file_list_linux() -> Option<Vec<PathBuf>> {
    // Linux 上文件管理器复制文件时，通常会将文件路径以 text/uri-list MIME 类型
    // 放入剪贴板。arboard 的 get_text() 有时能读到这些内容。
    // 我们尝试通过 xclip/xsel/wl-paste 命令行工具读取 text/uri-list
    let output = try_read_uri_list_x11()
        .or_else(|| try_read_uri_list_wayland());

    match output {
        Some(text) => parse_uri_list(&text),
        None => None,
    }
}

#[cfg(target_os = "linux")]
fn try_read_uri_list_x11() -> Option<String> {
    // 尝试使用 xclip 读取 text/uri-list
    match std::process::Command::new("xclip")
        .args(["-selection", "clipboard", "-t", "text/uri-list", "-o"])
        .output()
    {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout).to_string();
            if text.trim().is_empty() {
                None
            } else {
                debug!("xclip 读取到 uri-list: {}", text.trim());
                Some(text)
            }
        }
        _ => {
            // 尝试 xsel
            match std::process::Command::new("xsel")
                .args(["--clipboard", "--output"])
                .output()
            {
                Ok(output) if output.status.success() => {
                    let text = String::from_utf8_lossy(&output.stdout).to_string();
                    // xsel 不支持指定 MIME 类型，检查内容是否像 uri-list
                    if text.starts_with("file://") {
                        Some(text)
                    } else {
                        None
                    }
                }
                _ => None,
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn try_read_uri_list_wayland() -> Option<String> {
    match std::process::Command::new("wl-paste")
        .args(["--type", "text/uri-list"])
        .output()
    {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout).to_string();
            if text.trim().is_empty() {
                None
            } else {
                debug!("wl-paste 读取到 uri-list: {}", text.trim());
                Some(text)
            }
        }
        _ => None,
    }
}

#[cfg(target_os = "linux")]
fn parse_uri_list(text: &str) -> Option<Vec<PathBuf>> {
    let mut files = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        // 跳过注释行
        if trimmed.starts_with('#') || trimmed.is_empty() {
            continue;
        }
        if let Some(path_str) = trimmed.strip_prefix("file://") {
            // URL 解码
            let decoded = url_decode(path_str);
            let path = PathBuf::from(&decoded);
            if path.exists() {
                debug!("uri-list 解析文件: {}", decoded);
                files.push(path);
            } else {
                warn!("uri-list 文件不存在: {}", decoded);
            }
        }
    }
    if files.is_empty() {
        None
    } else {
        Some(files)
    }
}

#[cfg(target_os = "linux")]
fn url_decode(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut chars = input.bytes();
    while let Some(b) = chars.next() {
        if b == b'%' {
            let hi = chars.next();
            let lo = chars.next();
            if let (Some(h), Some(l)) = (hi, lo) {
                let hex_str = [h, l];
                if let Ok(s) = std::str::from_utf8(&hex_str) {
                    if let Ok(val) = u8::from_str_radix(s, 16) {
                        result.push(val as char);
                        continue;
                    }
                }
            }
            result.push('%');
        } else {
            result.push(b as char);
        }
    }
    result
}


/// 将文件路径列表写入系统剪贴板（原生格式）
/// Windows: 写入 CF_HDROP 格式，使文件可在资源管理器中粘贴
/// Linux: 同时设置 text/uri-list 和 x-special/gnome-copied-files 格式
pub fn set_clipboard_file_list(paths: &[PathBuf]) -> bool {
    if paths.is_empty() {
        return false;
    }
    #[cfg(target_os = "windows")]
    {
        set_clipboard_file_list_windows(paths)
    }
    #[cfg(target_os = "linux")]
    {
        set_clipboard_file_list_linux(paths)
    }
    #[cfg(not(any(target_os = "windows", target_os = "linux")))]
    {
        false
    }
}

#[cfg(target_os = "windows")]
fn set_clipboard_file_list_windows(paths: &[PathBuf]) -> bool {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE, GMEM_ZEROINIT,
    };
    use windows::Win32::System::Ole::CF_HDROP;

    unsafe {
        // 构建 UTF-16 编码的文件路径列表
        // DROPFILES 结构体 + 路径列表（每个路径以 null 结尾）+ 双 null 结尾
        let dropfiles_size = std::mem::size_of::<DROPFILES>();

        // 将所有路径转为 UTF-16
        let mut wide_paths: Vec<Vec<u16>> = Vec::new();
        let mut total_chars: usize = 0;
        for path in paths {
            let path_str = path.to_string_lossy();
            let wide: Vec<u16> = path_str.encode_utf16().chain(std::iter::once(0u16)).collect();
            total_chars += wide.len();
            wide_paths.push(wide);
        }
        total_chars += 1; // 额外的 null 终止符

        let total_size = dropfiles_size + total_chars * 2; // UTF-16 每个字符 2 字节

        if OpenClipboard(HWND::default()).is_err() {
            error!("无法打开剪贴板进行写入");
            return false;
        }

        let result = (|| -> bool {
            if EmptyClipboard().is_err() {
                error!("无法清空剪贴板");
                return false;
            }

            let hmem = match GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, total_size) {
                Ok(h) => h,
                Err(e) => {
                    error!("GlobalAlloc 失败: {}", e);
                    return false;
                }
            };

            let ptr = GlobalLock(hmem);
            if ptr.is_null() {
                error!("GlobalLock 失败");
                return false;
            }

            // 填充 DROPFILES 结构体
            let drop_files = ptr as *mut DROPFILES;
            (*drop_files).pFiles = dropfiles_size as u32;
            (*drop_files).pt_x = 0;
            (*drop_files).pt_y = 0;
            (*drop_files).fNC = windows::Win32::Foundation::BOOL(0);
            (*drop_files).fWide = windows::Win32::Foundation::BOOL(1); // UTF-16

            // 填充文件路径
            let mut dest = (ptr as *mut u8).add(dropfiles_size) as *mut u16;
            for wide in &wide_paths {
                for &ch in wide {
                    *dest = ch;
                    dest = dest.add(1);
                }
            }
            *dest = 0; // 双 null 终止

            let _ = GlobalUnlock(hmem);

            let handle: windows::Win32::Foundation::HANDLE = std::mem::transmute(hmem);
            match SetClipboardData(CF_HDROP.0 as u32, handle) {
                Ok(_) => {
                    info!("已将 {} 个文件路径写入剪贴板 (CF_HDROP)", paths.len());
                    true
                }
                Err(e) => {
                    error!("SetClipboardData 失败: {}", e);
                    false
                }
            }
        })();

        let _ = CloseClipboard();
        result
    }
}

#[cfg(target_os = "linux")]
fn set_clipboard_file_list_linux(paths: &[PathBuf]) -> bool {
    // 构建 text/uri-list 格式
    let uri_list: String = paths
        .iter()
        .map(|p| format!("file://{}", p.to_string_lossy()))
        .collect::<Vec<_>>()
        .join("\n");

    // 构建 x-special/gnome-copied-files 格式（copy\nfile://path1\nfile://path2）
    let gnome_files = format!(
        "copy\n{}",
        paths
            .iter()
            .map(|p| format!("file://{}", p.to_string_lossy()))
            .collect::<Vec<_>>()
            .join("\n")
    );

    // 尝试使用 xclip 同时设置多种 MIME 类型
    // xclip 支持通过管道设置指定 MIME 类型
    if set_clipboard_mime_xclip("text/uri-list", &uri_list)
        && set_clipboard_mime_xclip("x-special/gnome-copied-files", &gnome_files)
    {
        info!("已通过 xclip 将 {} 个文件路径写入剪贴板", paths.len());
        return true;
    }

    // 尝试 wl-copy（Wayland）
    if set_clipboard_wayland(paths) {
        info!("已通过 wl-copy 将 {} 个文件路径写入剪贴板", paths.len());
        return true;
    }

    warn!("无法将文件列表写入剪贴板：未找到可用的剪贴板工具");
    false
}

#[cfg(target_os = "linux")]
fn set_clipboard_mime_xclip(mime_type: &str, content: &str) -> bool {
    use std::io::Write;
    match std::process::Command::new("xclip")
        .args(["-selection", "clipboard", "-t", mime_type, "-i"])
        .stdin(std::process::Stdio::piped())
        .spawn()
    {
        Ok(mut child) => {
            if let Some(ref mut stdin) = child.stdin {
                if stdin.write_all(content.as_bytes()).is_err() {
                    return false;
                }
            }
            match child.wait() {
                Ok(status) => status.success(),
                Err(_) => false,
            }
        }
        Err(_) => false,
    }
}

#[cfg(target_os = "linux")]
fn set_clipboard_wayland(paths: &[PathBuf]) -> bool {
    use std::io::Write;

    // wl-copy 支持同时设置多种 MIME 类型
    let uri_list: String = paths
        .iter()
        .map(|p| format!("file://{}", p.to_string_lossy()))
        .collect::<Vec<_>>()
        .join("\n");

    match std::process::Command::new("wl-copy")
        .args(["--type", "text/uri-list"])
        .stdin(std::process::Stdio::piped())
        .spawn()
    {
        Ok(mut child) => {
            if let Some(ref mut stdin) = child.stdin {
                if stdin.write_all(uri_list.as_bytes()).is_err() {
                    return false;
                }
            }
            match child.wait() {
                Ok(status) => status.success(),
                Err(_) => false,
            }
        }
        Err(_) => false,
    }
}
