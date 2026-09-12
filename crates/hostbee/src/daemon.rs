//! `hostbee daemon` 保活服务：常驻循环定期 `refresh` 轮换 refreshToken 并原子落盘。
//!
//! 背景（spec #1「Update 2026-09-12」+ auth-flow.md §3）：后端 refreshToken 为
//! **7 天固定 TTL、无滑动续期**，闲置 7 天后 CLI 只能靠密码重登兜底。daemon 用远小于
//! TTL 的周期（默认每天 [`DEFAULT_INTERVAL_SECS`]）主动轮换，消除「闲置过期」场景；
//! CLI 侧 401 → refresh → 密码重登的兜底路径保持不变。
//!
//! 每轮循环：
//! 1. **重新读盘**配置——配置文件是 daemon 与并发 CLI 的唯一共享事实，CLI 侧轮换后
//!    daemon 用新值，不会拿已被服务端 revoke 的旧 token 去撞；
//! 2. 依次尝试 refreshToken 候选（env > config，各至多一次，与 [`crate::auth::execute`]
//!    同序），全部失败且存有 contact+password 时密码重登（复用 [`crate::auth::login_full`]，
//!    daemon 不引入新恢复机制）；
//! 3. 成功 → [`crate::auth::persist_pair`] 原子落盘（与 CLI 同一套写路径）；失败 →
//!    带时间戳的 stderr 日志 + 指数退避（[`backoff_delay`]），循环不因瞬态错误退出；
//! 4. 唯一致命条件：无任何可保活凭据（无 refreshToken 且无 contact+password）exit 1。
//!
//! 运行形态（[`main_entry`]）：
//! - bare `hostbee daemon` 在 systemd 机器上自动安装 user unit 并 enable --now 后退出
//!   （[`crate::systemd`]）；安装/启动失败降级前台循环——但服务确认 active 时直接
//!   退出，永不双跑（unit 的 ExecStart 恒为 `daemon --foreground`，服务进程不会递归安装）；
//! - 非 systemd 环境（macOS/容器）降级：一条明确提示后直接前台循环；
//! - `--foreground` 恒为前台循环。
//!
//! 输出契约：daemon 无 stdout JSON 契约，stdout 恒空；全部日志走 stderr，每行带
//! UTC 时间戳（[`utc_rfc3339`]）；致命错误以一行 JSON 收尾（同 CLI 错误契约）。

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use crate::auth::{self, AuthPair, REFRESH_TOKEN_ENV, Session};
use crate::config;
use crate::endpoint::{self, ENDPOINT_ENV};
use crate::error::CliError;
use crate::gql::{GraphqlTransport, UreqTransport};
use crate::systemd;

/// 默认轮换周期：每天一次（refreshToken 固定 7 天 TTL，一天远小于过期窗口）。
pub const DEFAULT_INTERVAL_SECS: u64 = 86_400;
/// 后端 refreshToken 的固定 TTL（604800s，auth-flow.md §3；无滑动续期）。
pub const REFRESH_TOKEN_TTL_SECS: u64 = 604_800;
/// 退避起始值：默认配置下 60s 起步（小于该值的 interval 用 interval 本身起步，
/// 保持测试等小周期场景的节奏）。
const BACKOFF_BASE_SECS: u64 = 60;
/// daemon 单次 HTTP 调用的整体超时（CLI 单次命令不设超时，进程短生命周期语义不同）。
const HTTP_TIMEOUT_SECS: u64 = 30;

// ---------- 纯逻辑（unit 测试覆盖） ----------

/// 校验 interval：0 拒绝（无意义的最小值 1s；e2e 用 1s 驱动快节奏循环）。
pub fn validate_interval(secs: u64) -> Result<(), String> {
    if secs == 0 {
        Err("interval 必须为正整数秒（最小 1）".to_owned())
    } else {
        Ok(())
    }
}

/// 失败退避时长：从 `min(60s, interval)` 起指数加倍，封顶 `max(interval/4, 起始值)`。
///
/// 取值理由：
/// - 封顶 interval/4 ⇒ 最坏重试节奏为每个周期窗口 4 次，对失败中的后端压力有界；
/// - 60s 起步让短瞬断（重启、网络抖动）在分钟级内被下一次重试覆盖，而 7 天 TTL
///   给足重试余量（默认配置 60s → 2m → 4m → … → 6h 封顶）；
/// - 起始值取 `min(60, interval)`：interval 本就小于 60s 时按其自身节奏退避，
///   小周期场景（e2e、容器）不会被 60s 拖慢。
pub fn backoff_delay(consecutive_failures: u32, interval_secs: u64) -> u64 {
    let start = BACKOFF_BASE_SECS.min(interval_secs);
    let cap = (interval_secs / 4).max(start);
    let shift = consecutive_failures.saturating_sub(1).min(63);
    start.saturating_mul(1u64 << shift).min(cap)
}

/// unix epoch 秒 → `YYYY-MM-DDTHH:MM:SSZ`（UTC，无时区库依赖）。
pub fn utc_rfc3339(unix_secs: u64) -> String {
    let days = unix_secs / 86_400;
    let secs_of_day = unix_secs % 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        secs_of_day / 3_600,
        secs_of_day / 60 % 60,
        secs_of_day % 60
    )
}

/// days-since-epoch → 公历年月日（Howard Hinnant 的 civil_from_days 算法）。
fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let z = days_since_epoch + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// 一行 daemon 日志：`[UTC RFC3339] 消息`，走 stderr（stdout 恒空）。
pub fn log(message: &str) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    eprintln!("[{}] {message}", utc_rfc3339(now));
}

/// 致命错误：一行 JSON 走 stderr + exit 1（同 CLI 错误契约）。
fn fatal(err: CliError) -> ExitCode {
    eprintln!("{}", err.stderr_json());
    ExitCode::from(crate::error::FAILURE_EXIT_CODE)
}

// ---------- 入口 ----------

/// `hostbee daemon` 入口（main.rs 直接分发；daemon 无 stdout JSON 输出）。
pub fn main_entry(interval_secs: u64, foreground: bool, endpoint_flag: Option<&str>) -> ExitCode {
    if let Err(msg) = validate_interval(interval_secs) {
        return fatal(CliError::Input(msg));
    }
    if interval_secs >= REFRESH_TOKEN_TTL_SECS {
        log(&format!(
            "警告：interval 为 {interval_secs}s，不小于 refreshToken 的 7 天固定 TTL——\
             token 很可能在两次轮换之间过期，建议显著调小"
        ));
    }
    // 启动时一次性预检（fail fast）；运行期错误只日志不退出
    let config_path = config::default_config_path();
    let file_config = match &config_path {
        Some(path) => match config::read_config(path) {
            Ok(config) => config,
            Err(msg) => return fatal(CliError::Config(msg)),
        },
        None => None,
    };
    let endpoint = match endpoint::resolve_endpoint(
        endpoint_flag,
        std::env::var(ENDPOINT_ENV).ok(),
        Ok(file_config.as_ref().and_then(|c| c.endpoint.clone())),
    ) {
        Ok(endpoint) => endpoint,
        Err(err) => return fatal(err),
    };
    let env_refresh = config::non_empty(std::env::var(REFRESH_TOKEN_ENV).ok());
    let config_has_refresh = file_config
        .as_ref()
        .is_some_and(|c| c.refresh_token.is_some());
    let config_has_login = file_config
        .as_ref()
        .is_some_and(|c| c.contact.is_some() && c.password.is_some());
    if !(env_refresh.is_some() || config_has_refresh || config_has_login) {
        return fatal(CliError::Input(
            "没有任何可保活的凭据（refreshToken 与 contact+password 均缺失）：请先运行 hostbee login"
                .to_owned(),
        ));
    }

    let loop_cfg = LoopConfig {
        interval_secs,
        endpoint,
        config_path,
    };
    if foreground {
        log(&format!(
            "前台保活启动：endpoint={} interval={}s",
            loop_cfg.endpoint, loop_cfg.interval_secs
        ));
        return run_loop(loop_cfg);
    }

    // ---- bare `hostbee daemon`：systemd 安装或降级 ----
    let systemctl = systemd::find_in_path("systemctl", std::env::var_os("PATH").as_deref());
    let systemd_up =
        systemd::detect_systemd(Path::new(systemd::SYSTEMD_RUN_DIR), systemctl.as_deref());
    if !systemd_up {
        log("未检测到 systemd，前台保活运行；Ctrl-C 退出");
        return run_loop(loop_cfg);
    }
    // systemd 服务进程读不到安装现场的 flag/env：endpoint 与凭据必须在配置文件里
    if config::non_empty(std::env::var(ENDPOINT_ENV).ok()).is_some() || endpoint_flag.is_some() {
        return fatal(CliError::Input(
            "systemd 服务无法继承 --endpoint/HOSTBEE_ENDPOINT：请先 hostbee login 把 \
             endpoint 写入 ~/.hostbee/config.toml 再运行 hostbee daemon"
                .to_owned(),
        ));
    }
    if !(config_has_refresh || config_has_login) {
        return fatal(CliError::Input(
            "systemd 服务无法继承环境变量 HOSTBEE_REFRESH_TOKEN：~/.hostbee/config.toml 中须有 \
             refresh_token 或 contact+password"
                .to_owned(),
        ));
    }
    let (Some(unit_dir), Ok(exe)) = (systemd::user_unit_dir(), std::env::current_exe()) else {
        return fatal(CliError::Input(
            "无法定位 unit 目录或当前二进制路径，降级方式：hostbee daemon --foreground".to_owned(),
        ));
    };
    let loginctl = systemd::find_in_path("loginctl", std::env::var_os("PATH").as_deref());
    match systemd::install_and_start(
        &exe.to_string_lossy(),
        interval_secs,
        &unit_dir,
        systemctl.as_deref().expect("systemd_up 时 systemctl 必在"),
        loginctl.as_deref(),
    ) {
        Ok(outcome) => {
            log(&format!(
                "已安装并启动 systemd 用户服务 {}（unit 文件：{}）",
                systemd::UNIT_NAME,
                outcome.unit_path.display()
            ));
            if let Some(warning) = outcome.linger_warning {
                log(&warning);
            }
            log(
                "保活循环由服务进程执行（ExecStart 带 --foreground），本命令到此退出；\
                 查看服务日志：journalctl --user -u hostbee-keepalive -f",
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            log(&format!("systemd 服务安装/启动失败：{e}"));
            log("降级为前台保活运行；Ctrl-C 退出");
            run_loop(loop_cfg)
        }
    }
}

// ---------- 前台循环 ----------

struct LoopConfig {
    interval_secs: u64,
    endpoint: String,
    config_path: Option<PathBuf>,
}

/// 前台保活循环：首轮立即执行（启动即验证凭据可用），此后每 interval 一轮；
/// SIGINT/SIGTERM 干净退出（exit 0）。永不因瞬态错误返回非 0。
fn run_loop(cfg: LoopConfig) -> ExitCode {
    let (signal_tx, signal_rx) = mpsc::channel::<()>();
    if ctrlc::set_handler(move || {
        let _ = signal_tx.send(());
    })
    .is_err()
    {
        log("警告：注册信号处理器失败，Ctrl-C/SIGTERM 将直接终止进程");
    }
    let transport = UreqTransport::with_timeout(Duration::from_secs(HTTP_TIMEOUT_SECS));
    let mut failures: u32 = 0;
    loop {
        let outcome = match load_cycle_session(&cfg.endpoint, cfg.config_path.as_deref()) {
            Ok(session) => refresh_cycle(&transport, &session),
            Err(msg) => CycleOutcome::Failed {
                notes: vec![format!("读取配置失败: {msg}")],
            },
        };
        let sleep_secs = match outcome {
            CycleOutcome::Rotated { via, pair } => {
                failures = 0;
                match via {
                    RefreshVia::Refresh => log("refresh 轮换成功，新 token 对已原子落盘"),
                    RefreshVia::Relogin => {
                        log("refresh 未成功，密码重登恢复会话成功，新 token 对已原子落盘")
                    }
                }
                if pair.refresh_token.is_none() {
                    log("警告：本次重登未获得新 refreshToken（账号未启用 TOTP），\
                         access token 约 10 分钟后过期，下轮须再次重登");
                }
                cfg.interval_secs
            }
            CycleOutcome::Failed { notes } => {
                failures += 1;
                let delay = backoff_delay(failures, cfg.interval_secs);
                log(&format!(
                    "保活失败（第 {failures} 次）：{}；退避 {delay}s 后重试",
                    notes.join("；")
                ));
                delay
            }
            CycleOutcome::NoCredentials => {
                log("保活凭据缺失（无 refreshToken 且无 contact+password），退出");
                return fatal(CliError::Input(
                    "没有任何可保活的凭据（refreshToken 与 contact+password 均缺失）：\
                     请先运行 hostbee login"
                        .to_owned(),
                ));
            }
        };
        match signal_rx.recv_timeout(Duration::from_secs(sleep_secs)) {
            Err(RecvTimeoutError::Timeout) => {}
            _ => {
                log("收到终止信号，退出");
                return ExitCode::SUCCESS;
            }
        }
    }
}

/// 每轮重建会话（重读配置文件 + env 覆盖），配置文件是并发 CLI 的共享事实。
fn load_cycle_session(endpoint: &str, config_path: Option<&Path>) -> Result<Session, String> {
    let file_config = match config_path {
        Some(path) => config::read_config(path)?,
        None => None,
    };
    Ok(Session::new(
        endpoint.to_owned(),
        file_config,
        config_path.map(PathBuf::from),
        std::env::var(REFRESH_TOKEN_ENV).ok(),
    ))
}

/// 一轮保活的轮换方式。
#[derive(Debug)]
enum RefreshVia {
    /// refreshToken 轮换成功。
    Refresh,
    /// refresh 全部失败后密码重登恢复。
    Relogin,
}

/// 一轮保活的结果。
#[derive(Debug)]
enum CycleOutcome {
    /// 轮换成功且新 token 对已原子落盘。
    Rotated { via: RefreshVia, pair: AuthPair },
    /// 有凭据但全部尝试失败（瞬态或服务端拒绝），应退避重试。
    Failed { notes: Vec<String> },
    /// 无任何凭据可尝试（致命）。
    NoCredentials,
}

/// 执行一轮保活：refresh 候选（env > config，同 [`crate::auth::execute`] 顺序）
/// → 密码重登兜底 → 原子落盘。落盘失败按失败处理（服务端已轮换，旧 refresh 已
/// 作废，下轮重登恢复）。
fn refresh_cycle<T: GraphqlTransport>(transport: &T, session: &Session) -> CycleOutcome {
    let mut notes: Vec<String> = Vec::new();
    let candidates = [
        session.refresh_token.as_deref(),
        session.fallback_refresh_token.as_deref(),
    ];
    for token in candidates.into_iter().flatten() {
        match auth::refresh(transport, &session.endpoint, token) {
            Ok(pair) => return rotated(session, pair, RefreshVia::Refresh),
            Err(err) => notes.push(format!("refresh 轮换失败（{}）", summarize(&err))),
        }
    }
    if let (Some(contact), Some(password)) =
        (session.contact.as_deref(), session.password.as_deref())
    {
        match auth::login_full(
            transport,
            &session.endpoint,
            contact,
            password,
            session.totp_secret.as_deref(),
        ) {
            Ok(pair) => return rotated(session, pair, RefreshVia::Relogin),
            Err(err) => notes.push(format!("密码重登失败（{}）", summarize(&err))),
        }
    }
    if notes.is_empty() {
        CycleOutcome::NoCredentials
    } else {
        CycleOutcome::Failed { notes }
    }
}

fn rotated(session: &Session, pair: AuthPair, via: RefreshVia) -> CycleOutcome {
    match auth::persist_pair(session, &pair) {
        Ok(()) => CycleOutcome::Rotated { via, pair },
        Err(msg) => CycleOutcome::Failed {
            notes: vec![format!(
                "新 token 对落盘失败（{msg}）——服务端已轮换，下轮须重新恢复会话"
            )],
        },
    }
}

/// 错误简短摘要（日志用）：GraphQL 错误取首条 message，其余取合成 message。
fn summarize(err: &CliError) -> String {
    let text = match err {
        CliError::GraphQlErrors(errors) => errors
            .first()
            .and_then(|e| e.get("message"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or("GraphQL errors")
            .to_owned(),
        other => other.message(),
    };
    crate::error::truncate(&text, 200)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::gql::request_body;
    use serde_json::{Value, json};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// 本地后端实测形状（见 auth.rs 测试）：HTTP 500 internal_error。
    const INTERNAL_ERROR: &str = r#"{"data":null,"errors":[{"message":"服务器内部错误，请稍后再试。","extensions":{"code":"messages.internal_error","status":500}}]}"#;

    // ---------- 参数校验与退避 ----------

    #[test]
    fn interval_校验_拒绝_0_接受正常值() {
        assert!(validate_interval(0).is_err());
        assert!(validate_interval(1).is_ok());
        assert!(validate_interval(DEFAULT_INTERVAL_SECS).is_ok());
    }

    #[test]
    fn 退避_默认周期_60s起步_指数加倍_封顶六小时() {
        // interval=86400：60s → 2m → 4m → … → 封顶 21600（6h）
        let expected = [
            60, 120, 240, 480, 960, 1_920, 3_840, 7_680, 15_360, 21_600, 21_600,
        ];
        for (i, want) in expected.iter().enumerate() {
            assert_eq!(
                backoff_delay(i as u32 + 1, 86_400),
                *want,
                "第 {} 次失败",
                i + 1
            );
        }
        // 长期失败恒封顶，不溢出
        assert_eq!(backoff_delay(100, 86_400), 21_600);
        assert_eq!(backoff_delay(u32::MAX, 86_400), 21_600);
    }

    #[test]
    fn 退避_小周期_start等于interval_封顶不小于start() {
        // interval=1（e2e 节奏）：退避恒 1s，不被 60s 基数拖慢
        assert_eq!(backoff_delay(1, 1), 1);
        assert_eq!(backoff_delay(9, 1), 1);
        // interval=200：起步已是 60s 基数；interval/4=50 < 60，封顶不小于起步值
        assert_eq!(backoff_delay(1, 200), 60);
        assert_eq!(backoff_delay(5, 200), 60);
        // interval=3600：60s 起步，封顶 900
        assert_eq!(backoff_delay(1, 3_600), 60);
        assert_eq!(backoff_delay(4, 3_600), 480);
        assert_eq!(backoff_delay(5, 3_600), 900);
        assert_eq!(backoff_delay(6, 3_600), 900);
    }

    // ---------- 时间戳 ----------

    #[test]
    fn utc_rfc3339_已知向量_含闰年() {
        assert_eq!(utc_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc_rfc3339(951_782_400), "2000-02-29T00:00:00Z"); // 闰日
        assert_eq!(utc_rfc3339(1_789_202_482), "2026-09-12T08:41:22Z");
        assert_eq!(utc_rfc3339(2_000_000_000), "2033-05-18T03:33:20Z");
        // 日内时分秒边界
        assert_eq!(utc_rfc3339(86_399), "1970-01-01T23:59:59Z");
        assert_eq!(utc_rfc3339(86_400), "1970-01-02T00:00:00Z");
    }

    // ---------- refresh_cycle（脚本化传输） ----------

    /// 脚本化传输：按序弹出预设回复；记录每个请求的类别与 refresh 变量。
    struct FakeTransport {
        replies: Mutex<VecDeque<(u16, String)>>,
        calls: Mutex<Vec<String>>,
    }

    impl FakeTransport {
        fn new(replies: &[(u16, &str)]) -> Self {
            Self {
                replies: Mutex::new(replies.iter().map(|&(s, b)| (s, b.to_owned())).collect()),
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl GraphqlTransport for FakeTransport {
        fn post(
            &self,
            _endpoint: &str,
            _auth: Option<&str>,
            body: &Value,
        ) -> Result<(u16, String), String> {
            let query = body.get("query").and_then(Value::as_str).unwrap_or("");
            let kind = if query.contains("login(contact") {
                "login"
            } else if query.contains("refresh(refreshToken") {
                "refresh"
            } else if query.contains("verifyTotp(") {
                "verifyTotp"
            } else {
                "gql"
            };
            let refresh_var = body
                .pointer("/variables/refreshToken")
                .and_then(Value::as_str)
                .unwrap_or("-");
            self.calls
                .lock()
                .unwrap()
                .push(format!("{kind}|{refresh_var}"));
            let reply = self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("循环发出了未预期的请求（回复脚本已耗尽）");
            Ok(reply)
        }
    }

    /// data.<field> 的 AuthOutput 响应体。
    fn pair_response(field: &str, access: &str, refresh: Option<&str>) -> String {
        let mut data = serde_json::Map::new();
        data.insert(
            field.to_owned(),
            json!({
                "accessToken": access,
                "refreshToken": refresh,
                "userInfo": {
                    "id": "u1", "email": "daemon@hostbee.test", "hasTotp": false,
                    "hasPasskey": false, "emailVerified": true, "phoneVerified": false,
                    "allowTicket": true
                }
            }),
        );
        serde_json::to_string(&json!({ "data": data })).unwrap()
    }

    fn session(dir: &tempfile::TempDir, refresh: Option<&str>, password: Option<&str>) -> Session {
        let config = Config {
            endpoint: Some("http://stub".to_owned()),
            contact: Some("daemon@hostbee.test".to_owned()),
            password: password.map(str::to_owned),
            refresh_token: refresh.map(str::to_owned),
            ..Config::default()
        };
        Session::new(
            "http://stub".to_owned(),
            Some(config),
            Some(dir.path().join("config.toml")),
            None,
        )
    }

    #[test]
    fn 保活轮_首选refresh成功_落盘并保留账号信息() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        let session = session(&dir, Some("ref-old"), Some("pw"));
        let transport =
            FakeTransport::new(&[(200, &pair_response("refresh", "acc-new", Some("ref-new")))]);
        let outcome = refresh_cycle(&transport, &session);
        assert!(matches!(
            outcome,
            CycleOutcome::Rotated {
                via: RefreshVia::Refresh,
                ..
            }
        ));
        assert_eq!(transport.calls(), vec!["refresh|ref-old"]);
        let config = config::read_config(&path).unwrap().unwrap();
        assert_eq!(config.access_token.as_deref(), Some("acc-new"));
        assert_eq!(config.refresh_token.as_deref(), Some("ref-new"));
        assert_eq!(config.contact.as_deref(), Some("daemon@hostbee.test"));
        assert_eq!(config.password.as_deref(), Some("pw"));
        assert_eq!(config.endpoint.as_deref(), Some("http://stub"));
    }

    #[test]
    fn 保活轮_env候选失效后_config候选兜底成功() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = Config {
            endpoint: Some("http://stub".to_owned()),
            contact: Some("daemon@hostbee.test".to_owned()),
            password: Some("pw".to_owned()),
            refresh_token: Some("ref-file".to_owned()),
            ..Config::default()
        };
        let session = Session::new(
            "http://stub".to_owned(),
            Some(config),
            Some(dir.path().join("config.toml")),
            Some("ref-env-stale".to_owned()),
        );
        let transport = FakeTransport::new(&[
            (500, INTERNAL_ERROR), // env 旧值已被服务端 revoke
            (200, &pair_response("refresh", "acc-new", Some("ref-new"))),
        ]);
        let outcome = refresh_cycle(&transport, &session);
        assert!(matches!(
            outcome,
            CycleOutcome::Rotated {
                via: RefreshVia::Refresh,
                ..
            }
        ));
        assert_eq!(
            transport.calls(),
            vec!["refresh|ref-env-stale", "refresh|ref-file"]
        );
    }

    #[test]
    fn 保活轮_refresh全失败_密码重登恢复_非totp保留旧refresh() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        let session = session(&dir, Some("ref-dead"), Some("pw"));
        let transport = FakeTransport::new(&[
            (500, INTERNAL_ERROR),
            (200, &pair_response("login", "acc-login", None)),
        ]);
        let outcome = refresh_cycle(&transport, &session);
        assert!(matches!(
            outcome,
            CycleOutcome::Rotated {
                via: RefreshVia::Relogin,
                ..
            }
        ));
        assert_eq!(transport.calls(), vec!["refresh|ref-dead", "login|-"]);
        let config = config::read_config(&path).unwrap().unwrap();
        assert_eq!(config.access_token.as_deref(), Some("acc-login"));
        // 非 TOTP 重登拿不到新 refresh：保留旧值，不做无谓丢失
        assert_eq!(config.refresh_token.as_deref(), Some("ref-dead"));
    }

    #[test]
    fn 保活轮_全部失败_汇总两条注记() {
        let dir = tempfile::TempDir::new().unwrap();
        let session = session(&dir, Some("ref-dead"), Some("pw-wrong"));
        let transport = FakeTransport::new(&[
            (500, INTERNAL_ERROR),
            (500, INTERNAL_ERROR), // 密码错误 → 后端实测同为 500
        ]);
        let outcome = refresh_cycle(&transport, &session);
        let CycleOutcome::Failed { notes } = outcome else {
            panic!("应为 Failed: {outcome:?}")
        };
        assert_eq!(notes.len(), 2);
        assert!(notes[0].contains("refresh 轮换失败"));
        assert!(notes[1].contains("密码重登失败"));
    }

    #[test]
    fn 保活轮_无任何凭据_零请求_致命() {
        let dir = tempfile::TempDir::new().unwrap();
        let session = session(&dir, None, None);
        let transport = FakeTransport::new(&[]);
        assert!(matches!(
            refresh_cycle(&transport, &session),
            CycleOutcome::NoCredentials
        ));
        assert_eq!(transport.calls(), Vec::<String>::new());
    }

    #[test]
    fn 保活轮_落盘失败_按失败处理_服务端已轮换() {
        let dir = tempfile::TempDir::new().unwrap();
        // config_path 指向一个已存在的目录：write_config_atomic 的 rename 必然失败
        let config = Config {
            endpoint: Some("http://stub".to_owned()),
            contact: Some("daemon@hostbee.test".to_owned()),
            password: Some("pw".to_owned()),
            refresh_token: Some("ref-old".to_owned()),
            ..Config::default()
        };
        let session = Session::new(
            "http://stub".to_owned(),
            Some(config),
            Some(dir.path().to_owned()),
            None,
        );
        let transport =
            FakeTransport::new(&[(200, &pair_response("refresh", "acc-new", Some("ref-new")))]);
        let outcome = refresh_cycle(&transport, &session);
        let CycleOutcome::Failed { notes } = outcome else {
            panic!("落盘失败应按 Failed 处理: {outcome:?}")
        };
        assert!(
            notes.iter().any(|n| n.contains("落盘失败")),
            "应含落盘失败注记: {notes:?}"
        );
    }

    #[test]
    fn 请求体_带refresh变量() {
        // 传输层记录的 refresh|<token> 形状依赖 request_body 的 variables 透传，
        // 这里锚定一次组装形状，防止 transport 记录口径失真。
        let body = request_body(
            "mutation { refresh(refreshToken: $refreshToken) { accessToken } }",
            Some(json!({ "refreshToken": "x" })),
        );
        assert_eq!(body["variables"]["refreshToken"], json!("x"));
    }
}
