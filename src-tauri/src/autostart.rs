// ---------------- 开机自启动（注册表主方案 + 启动文件夹回退） ----------------
// 取代 tauri-plugin-autostart（auto-launch 0.5）：后者把任务管理器
// StartupApproved 键的写入当作致命错误，Run 键或该键的 ACL 被安全软件 /
// 组策略收紧的机器上会报「拒绝访问 (os error 5)」，提权也绕不过
// （HKCU 的权限看的是用户，不是进程令牌）。见 issue #4。
// 这里注册表写入失败时回退到在用户启动文件夹创建快捷方式，两条路总有一条能走通。

use std::os::windows::process::CommandExt;
use std::path::PathBuf;

use winreg::enums::{HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE};
use winreg::enums::RegType::REG_BINARY;
use winreg::{RegKey, RegValue};

const RUN_KEY: &str = "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Run";
/// 任务管理器「启动应用」的启用状态键；值缺失视为启用
const APPROVED_RUN_KEY: &str =
    "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved\\Run";
/// 启动文件夹快捷方式对应的启用状态键（同上）
const APPROVED_FOLDER_KEY: &str =
    "SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved\\StartupFolder";
const VALUE_NAME: &str = "QuickScan";
const LNK_NAME: &str = "QuickScan.lnk";
/// StartupApproved 的「已启用」值：首字节 0x02，其余全 0
const APPROVED_ENABLED: [u8; 12] = [0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
/// CREATE_NO_WINDOW：PowerShell 是控制台程序，不遮挡会闪黑窗
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn hkcu() -> RegKey {
    RegKey::predef(HKEY_CURRENT_USER)
}

/// 用户启动文件夹中的快捷方式路径（shell:startup）
fn startup_lnk_path() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|roaming| {
        PathBuf::from(roaming)
            .join(r"Microsoft\Windows\Start Menu\Programs\Startup")
            .join(LNK_NAME)
    })
}

/// 该启动项在任务管理器中是否处于启用状态（键/值缺失 = 启用）
fn approved(subkey: &str, value: &str) -> bool {
    let Ok(key) = hkcu().open_subkey_with_flags(subkey, KEY_READ) else {
        return true;
    };
    let Ok(raw) = key.get_raw_value(value) else {
        return true;
    };
    // 后 8 字节全 0 = 启用；被禁用时这 8 字节是禁用时间戳
    raw.bytes.iter().rev().take(8).all(|&b| b == 0)
}

fn write_run_key() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("无法定位程序路径: {e}"))?;
    let key = hkcu()
        .open_subkey_with_flags(RUN_KEY, KEY_SET_VALUE)
        .map_err(|e| format!("打开注册表 Run 键失败: {e}"))?;
    // 路径含空格（如 Program Files）必须加引号，否则开机执行时会被截断
    key.set_value(VALUE_NAME, &format!("\"{}\"", exe.display()))
        .map_err(|e| format!("写入注册表失败: {e}"))?;
    // StartupApproved 只影响任务管理器里的显示状态，写不进去不算失败
    if let Ok(approved_key) = hkcu().open_subkey_with_flags(APPROVED_RUN_KEY, KEY_SET_VALUE) {
        let _ = approved_key.set_raw_value(
            VALUE_NAME,
            &RegValue {
                vtype: REG_BINARY,
                bytes: APPROVED_ENABLED.to_vec(),
            },
        );
    }
    Ok(())
}

/// PowerShell 单引号字符串内用两个单引号转义
fn ps_quote(s: &str) -> String {
    s.replace('\'', "''")
}

fn create_startup_shortcut() -> Result<(), String> {
    let lnk = startup_lnk_path().ok_or("无法定位启动文件夹")?;
    let exe = std::env::current_exe().map_err(|e| format!("无法定位程序路径: {e}"))?;
    let dir = exe
        .parent()
        .unwrap_or(std::path::Path::new(""))
        .display()
        .to_string();
    let script = format!(
        "$s=(New-Object -ComObject WScript.Shell).CreateShortcut('{}');$s.TargetPath='{}';$s.WorkingDirectory='{}';$s.Save()",
        ps_quote(&lnk.display().to_string()),
        ps_quote(&exe.display().to_string()),
        ps_quote(&dir),
    );
    let out = std::process::Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            &script,
        ])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| format!("启动 PowerShell 失败: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "创建快捷方式失败: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

pub fn enable() -> Result<(), String> {
    match write_run_key() {
        Ok(()) => Ok(()),
        Err(reg_err) => create_startup_shortcut()
            .map_err(|lnk_err| format!("注册表方式失败（{reg_err}），启动文件夹回退也失败：{lnk_err}")),
    }
}

pub fn disable() -> Result<(), String> {
    // 值本来就不存在 = 已是关闭状态，不算错误
    let mut last_err = None;
    if let Err(e) = hkcu()
        .open_subkey_with_flags(RUN_KEY, KEY_SET_VALUE)
        .and_then(|k| k.delete_value(VALUE_NAME))
    {
        if e.kind() != std::io::ErrorKind::NotFound {
            last_err = Some(format!("删除注册表自启动项失败: {e}"));
        }
    }
    if let Some(lnk) = startup_lnk_path() {
        if let Err(e) = std::fs::remove_file(&lnk) {
            if e.kind() != std::io::ErrorKind::NotFound && last_err.is_none() {
                last_err = Some(format!("删除启动文件夹快捷方式失败: {e}"));
            }
        }
    }
    last_err.map_or(Ok(()), Err)
}

pub fn is_enabled() -> Result<bool, String> {
    let by_reg = hkcu()
        .open_subkey_with_flags(RUN_KEY, KEY_READ)
        .ok()
        .is_some_and(|k| k.get_value::<String, _>(VALUE_NAME).is_ok())
        && approved(APPROVED_RUN_KEY, VALUE_NAME);
    let by_lnk = startup_lnk_path().is_some_and(|p| p.is_file())
        && approved(APPROVED_FOLDER_KEY, LNK_NAME);
    Ok(by_reg || by_lnk)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enable_disable_roundtrip() {
        disable().expect("清理旧状态失败");
        assert!(!is_enabled().unwrap(), "初始应为关闭");

        enable().expect("启用失败");
        assert!(is_enabled().unwrap(), "启用后应报告已开启");

        disable().expect("禁用失败");
        assert!(!is_enabled().unwrap(), "禁用后应报告已关闭");
    }

    #[test]
    fn startup_folder_fallback() {
        disable().expect("清理旧状态失败");
        create_startup_shortcut().expect("创建启动文件夹快捷方式失败");
        assert!(is_enabled().unwrap(), "快捷方式存在时应报告已开启");
        disable().expect("移除快捷方式失败");
        assert!(!is_enabled().unwrap(), "移除后应报告已关闭");
    }
}
