//! systemd 用户服务的检测与自动安装（`hostbee daemon` 的非 foreground 路径）。
//!
//! 全部输入（run dir、unit 目录、systemctl/loginctl 路径）都作为参数注入：
//! unit 测试用临时目录里的假 systemctl 脚本驱动完整安装流程，不依赖真实 systemd
//! （真实 enable 的最终效果只能在 Linux systemd 机器上人工验证）。
//!
//! 防递归（fork-bomb）：unit 的 ExecStart 恒为 `daemon --foreground`，服务进程走
//! 前台循环，不会再次进入安装分支；bare `hostbee daemon` 只安装、enable 并退出。
//!
//! 安全设置按 ADR-0001 保持最小：不引入任何沙箱/加固指令——unit 归用户自己所有，
//! 与 CLI 进程同权限运行，CLI 侧不设防则服务侧同样不设防。

use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

/// 自动安装的 systemd user unit 名。
pub const UNIT_NAME: &str = "hostbee-keepalive.service";
/// systemd user 实例在位的标记目录（`systemctl --user` 可用的前提）。
pub const SYSTEMD_RUN_DIR: &str = "/run/systemd/system";

/// systemd 检测（spec：run dir 在位 + PATH 上找得到 systemctl，两者都参数化）。
pub fn detect_systemd(run_dir: &Path, systemctl: Option<&Path>) -> bool {
    run_dir.is_dir() && systemctl.is_some()
}

/// 在 PATH 环境变量里找可执行文件（逐目录扫描；文件须存在且带执行位）。
pub fn find_in_path(name: &str, path_var: Option<&OsStr>) -> Option<PathBuf> {
    std::env::split_paths(path_var?)
        .map(|dir| dir.join(name))
        .find(|candidate| is_executable(candidate))
}

/// unix 可执行判定：文件存在（`metadata` 跟随符号链接）且带任意执行位。
fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|meta| meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// user unit 目录：`XDG_CONFIG_HOME/systemd/user`，未设置时回落 `~/.config/systemd/user`。
pub fn resolve_unit_dir(xdg_config_home: Option<&OsStr>, home: Option<&Path>) -> Option<PathBuf> {
    if let Some(xdg) = xdg_config_home
        && !xdg.is_empty()
    {
        return Some(Path::new(xdg).join("systemd").join("user"));
    }
    home.map(|h| h.join(".config").join("systemd").join("user"))
}

/// 当前用户的 user unit 目录（生产入口；拿不到 home 时 None）。
pub fn user_unit_dir() -> Option<PathBuf> {
    resolve_unit_dir(
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
        std::env::home_dir().as_deref(),
    )
}

/// unit 文件内容（golden 断言见测试）：
/// - `ExecStart` 用安装时的二进制绝对路径（`current_exe`）+ `--foreground`——
///   服务进程直接跑循环，不会递归安装；移动二进制后需重新运行 `hostbee daemon` 更新；
/// - `Restart=always` + `RestartSec=10s`：daemon 循环自身已对瞬态错误退避、
///   正常情况永不退出，systemd 只兜底真实崩溃（OOM、panic），10s 防紧崩溃循环；
/// - 网络就绪后启动（`network-online.target`）提高首轮 refresh 成功率。
pub fn unit_file_content(exe: &str, interval_secs: u64) -> String {
    format!(
        "# 由 hostbee daemon 自动生成；改动周期后重新运行 hostbee daemon 即可更新。\n\
         [Unit]\n\
         Description=hostbee keepalive——refreshToken 定期轮换\n\
         Wants=network-online.target\n\
         After=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart=\"{exe}\" daemon --foreground --interval {interval_secs}\n\
         Restart=always\n\
         RestartSec=10s\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

/// 原子写 unit 文件（同目录临时文件 + rename，与配置文件同一套语义）。
pub fn write_unit_atomic(path: &Path, content: &str) -> Result<(), String> {
    let Some(parent) = path.parent() else {
        return Err(format!("unit 路径 {} 无父目录", path.display()));
    };
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("创建 unit 目录 {} 失败: {e}", parent.display()))?;
    let tmp = parent.join(format!(".{UNIT_NAME}.tmp"));
    std::fs::write(&tmp, content).map_err(|e| format!("写入 {} 失败: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| format!("rename {} -> {} 失败: {e}", tmp.display(), path.display()))
}

/// 安装成功（服务 active）的结果。
#[derive(Debug)]
pub struct InstallOutcome {
    /// 落地的 unit 文件路径。
    pub unit_path: PathBuf,
    /// linger 启用失败时的提示（不阻断安装成功；None = linger 已启用或无需提示）。
    pub linger_warning: Option<String>,
}

/// 完整安装流程，返回服务最终是否 active：
/// 1. unit 内容与现文件一致 → 幂等重装：只 `enable --now`，不动运行中的服务；
/// 2. 内容有变化 → 原子写盘 → `daemon-reload` → `enable --now` → `try-restart`
///    （try-restart 仅在服务原本 active 时重启，让新 ExecStart 生效）；
/// 3. 末尾以 `is-active --quiet` 确认服务真的在跑——不 active 即 Err，
///    调用方据此降级前台循环（确认 active 时调用方直接退出，不会双跑）。
pub fn install_and_start(
    exe: &str,
    interval_secs: u64,
    unit_dir: &Path,
    systemctl: &Path,
    loginctl: Option<&Path>,
) -> Result<InstallOutcome, String> {
    std::fs::create_dir_all(unit_dir)
        .map_err(|e| format!("创建 unit 目录 {} 失败: {e}", unit_dir.display()))?;
    let unit_path = unit_dir.join(UNIT_NAME);
    let desired = unit_file_content(exe, interval_secs);
    let unchanged = std::fs::read_to_string(&unit_path).unwrap_or_default() == desired;

    let mut errors: Vec<String> = Vec::new();
    if unchanged {
        if let Err(e) = run_systemctl(systemctl, &["enable", "--now", UNIT_NAME]) {
            errors.push(e);
        }
    } else {
        write_unit_atomic(&unit_path, &desired)?;
        for args in [
            &["daemon-reload"][..],
            &["enable", "--now", UNIT_NAME][..],
            &["try-restart", UNIT_NAME][..],
        ] {
            if let Err(e) = run_systemctl(systemctl, args) {
                errors.push(e);
            }
        }
    }
    if !is_active(systemctl) {
        errors.push(format!("服务 {UNIT_NAME} 未处于 active 状态"));
    }
    if !errors.is_empty() {
        return Err(errors.join("；"));
    }
    Ok(InstallOutcome {
        unit_path,
        linger_warning: enable_linger(loginctl),
    })
}

/// 执行一条 `systemctl --user <args>`；失败返回含退出码与 stderr 的错误。
fn run_systemctl(systemctl: &Path, args: &[&str]) -> Result<(), String> {
    let output = Command::new(systemctl)
        .arg("--user")
        .args(args)
        .output()
        .map_err(|e| format!("无法执行 {}: {e}", systemctl.display()))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    let exit = output.status.code().unwrap_or(-1);
    let detail = if stderr.is_empty() {
        String::new()
    } else {
        format!("：{stderr}")
    };
    Err(format!(
        "systemctl --user {} 失败（exit {exit}）{detail}",
        args.join(" ")
    ))
}

/// `is-active --quiet`：服务是否在跑（systemctl 不存在或执行失败一律视为不在跑）。
fn is_active(systemctl: &Path) -> bool {
    Command::new(systemctl)
        .arg("--user")
        .args(["is-active", "--quiet", UNIT_NAME])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// user unit 的「开机自启（无需登录）」需要 linger，尽力而为：
/// 失败/缺 loginctl 不影响安装结果，只返回提示文本。
fn enable_linger(loginctl: Option<&Path>) -> Option<String> {
    let Some(loginctl) = loginctl else {
        return Some(
            "未找到 loginctl：无法启用 linger，机器重启后需登录一次才会启动服务\
             （可手动执行 loginctl enable-linger）"
                .to_owned(),
        );
    };
    let status = Command::new(loginctl).arg("enable-linger").status();
    match status {
        Ok(s) if s.success() => None,
        Ok(_) => Some(
            "loginctl enable-linger 失败：机器重启后需登录一次才会启动服务（可手动重试）"
                .to_owned(),
        ),
        Err(e) => Some(format!(
            "无法执行 loginctl（{e}）：无法启用 linger，机器重启后需登录一次才会启动服务"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// 写一个假可执行（shell 脚本），返回其路径。
    fn fake_bin(dir: &Path, name: &str, script: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, script).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn unit_文件内容_golden() {
        assert_eq!(
            unit_file_content("/usr/local/bin/hostbee", 86_400),
            "# 由 hostbee daemon 自动生成；改动周期后重新运行 hostbee daemon 即可更新。\n\
             [Unit]\n\
             Description=hostbee keepalive——refreshToken 定期轮换\n\
             Wants=network-online.target\n\
             After=network-online.target\n\
             \n\
             [Service]\n\
             Type=simple\n\
             ExecStart=\"/usr/local/bin/hostbee\" daemon --foreground --interval 86400\n\
             Restart=always\n\
             RestartSec=10s\n\
             \n\
             [Install]\n\
             WantedBy=default.target\n"
        );
    }

    #[test]
    fn 安装_首次_写盘_reload_enable_restart_并确认_active() {
        let dir = TempDir::new().unwrap();
        let unit_dir = dir.path().join("units");
        let bin_dir = dir.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let log = dir.path().join("calls.log");
        // 假 systemctl：记录收到的参数（--user 由真实调用方附加），恒 exit 0
        let systemctl = fake_bin(
            &bin_dir,
            "systemctl",
            &format!("#!/bin/sh\necho \"$@\" >> {}\nexit 0\n", log.display()),
        );

        let outcome = install_and_start("/opt/hostbee", 3_600, &unit_dir, &systemctl, None)
            .expect("安装应成功");
        assert_eq!(outcome.unit_path, unit_dir.join(UNIT_NAME));
        // unit 已落盘且为 golden 内容
        let on_disk = fs::read_to_string(&outcome.unit_path).unwrap();
        assert_eq!(on_disk, unit_file_content("/opt/hostbee", 3_600));
        // systemctl 调用序列：首次安装走 reload → enable → try-restart → is-active
        // （假脚本记录的是含 --user 前缀的完整参数行）
        let calls = fs::read_to_string(&log).unwrap();
        assert_eq!(
            calls,
            "--user daemon-reload\n\
             --user enable --now hostbee-keepalive.service\n\
             --user try-restart hostbee-keepalive.service\n\
             --user is-active --quiet hostbee-keepalive.service\n"
        );
        // 未提供 loginctl → linger 有提示
        assert!(outcome.linger_warning.is_some());
    }

    #[test]
    fn 安装_unit内容未变_幂等_只确保enable_不reload不restart() {
        let dir = TempDir::new().unwrap();
        let unit_dir = dir.path().join("units");
        fs::create_dir_all(&unit_dir).unwrap();
        // 预置与期望一致的 unit 文件
        fs::write(
            unit_dir.join(UNIT_NAME),
            unit_file_content("/opt/hostbee", 3_600),
        )
        .unwrap();
        let bin_dir = dir.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let log = dir.path().join("calls.log");
        let systemctl = fake_bin(
            &bin_dir,
            "systemctl",
            &format!("#!/bin/sh\necho \"$@\" >> {}\nexit 0\n", log.display()),
        );

        let outcome = install_and_start("/opt/hostbee", 3_600, &unit_dir, &systemctl, None)
            .expect("幂等重装应成功");
        let calls = fs::read_to_string(&log).unwrap();
        // 不写盘（内容未变）、不 daemon-reload、不 restart
        assert_eq!(
            calls,
            "--user enable --now hostbee-keepalive.service\n\
             --user is-active --quiet hostbee-keepalive.service\n"
        );
        assert!(outcome.linger_warning.is_some());
    }

    #[test]
    fn 安装_服务未active_汇总各步失败返回_err() {
        let dir = TempDir::new().unwrap();
        let unit_dir = dir.path().join("units");
        let bin_dir = dir.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        // 恒 exit 1 的 systemctl：所有步骤失败，is-active 也不通过
        let systemctl = fake_bin(&bin_dir, "systemctl", "#!/bin/sh\nexit 1\n");

        let err =
            install_and_start("/opt/hostbee", 3_600, &unit_dir, &systemctl, None).unwrap_err();
        assert!(
            err.contains("daemon-reload"),
            "应包含 daemon-reload 失败信息: {err}"
        );
        assert!(
            err.contains("enable --now"),
            "应包含 enable 失败信息: {err}"
        );
        assert!(
            err.contains("未处于 active 状态"),
            "应包含未 active 结论: {err}"
        );
    }

    #[test]
    fn systemd_检测_两条件缺一不可() {
        let dir = TempDir::new().unwrap();
        let fake_ctl = dir.path().join("systemctl");
        fs::write(&fake_ctl, "#!/bin/sh\n").unwrap();
        assert!(detect_systemd(dir.path(), Some(&fake_ctl)));
        assert!(
            !detect_systemd(dir.path(), None),
            "PATH 上无 systemctl 不算"
        );
        let missing = dir.path().join("nope");
        assert!(
            !detect_systemd(&missing, Some(&fake_ctl)),
            "run dir 不在不算"
        );
    }

    #[test]
    fn path查找_只认带执行位的文件() {
        use std::ffi::OsString;
        let dir = TempDir::new().unwrap();
        let ctl = dir.path().join("systemctl");
        fs::write(&ctl, "#!/bin/sh\n").unwrap();
        fs::set_permissions(&ctl, fs::Permissions::from_mode(0o755)).unwrap();
        let path_var = OsString::from(format!("/nonexistent:{}", dir.path().display()));

        assert_eq!(
            find_in_path("systemctl", Some(&path_var)),
            Some(ctl.clone())
        );
        // 去掉执行位 → 不认
        fs::set_permissions(&ctl, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(find_in_path("systemctl", Some(&path_var)), None);
        // PATH 里没有 / 未设 PATH → None
        assert_eq!(find_in_path("loginctl", Some(&path_var)), None);
        assert_eq!(find_in_path("systemctl", None), None);
    }

    #[test]
    fn unit目录_xdg优先_空串视为未设置_缺home为none() {
        let home = Path::new("/home/agent");
        assert_eq!(
            resolve_unit_dir(Some(OsStr::new("/xdg")), Some(home)),
            Some(PathBuf::from("/xdg/systemd/user"))
        );
        assert_eq!(
            resolve_unit_dir(None, Some(home)),
            Some(PathBuf::from("/home/agent/.config/systemd/user"))
        );
        assert_eq!(
            resolve_unit_dir(Some(OsStr::new("")), Some(home)),
            Some(PathBuf::from("/home/agent/.config/systemd/user"))
        );
        assert_eq!(resolve_unit_dir(None, None), None);
    }

    #[test]
    fn linger_成功时无提示_假loginctl_exit_0() {
        let dir = TempDir::new().unwrap();
        let loginctl = fake_bin(dir.path(), "loginctl", "#!/bin/sh\nexit 0\n");
        assert_eq!(enable_linger(Some(&loginctl)), None);
        let failing = fake_bin(dir.path(), "loginctl-bad", "#!/bin/sh\nexit 1\n");
        assert!(enable_linger(Some(&failing)).is_some());
        assert!(enable_linger(None).is_some());
    }
}
