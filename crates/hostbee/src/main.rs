//! hostbee 二进制入口：参数解析 + 输入输出编排。
//!
//! 输出契约：stdout 仅打印结果 JSON（compact 单行）；所有错误打印到
//! stderr（一行 `{"errors":[...]}` JSON）并以 exit code 1 退出。
//! 诊断性警告（如配置写盘失败）也走 stderr 纯文本，stdout 永远只有结果 JSON。

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use hostbee::auth::{self, CONTACT_ENV, PASSWORD_ENV, REFRESH_TOKEN_ENV, Session, TOTP_SECRET_ENV};
use hostbee::config::{self, Config};
use hostbee::endpoint::{self, ENDPOINT_ENV};
use hostbee::error::{CliError, FAILURE_EXIT_CODE};
use hostbee::gql;
use serde_json::Value;

/// rustybee 后端的 agent 专用 CLI。命令面由 schema codegen 生成（ticket #4），
/// `gql` 为透传任意 GraphQL document 的逃生门。
#[derive(Parser)]
#[command(name = "hostbee", version, about, disable_help_subcommand = true)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 交互式登录：凭据明文持久化到 ~/.hostbee/config.toml（ADR-0001）
    Login {
        /// 联系方式（邮箱/手机号）；缺省时按 flag > HOSTBEE_CONTACT > 交互输入解析
        #[arg(long)]
        contact: Option<String>,
        /// 密码；缺省时按 flag > HOSTBEE_PASSWORD > 交互输入解析
        #[arg(long)]
        password: Option<String>,
        /// TOTP 密钥（hex）；TOTP 账号登录换取 token 对必需，
        /// 缺省时按 flag > HOSTBEE_TOTP_SECRET > 配置文件已有值解析
        #[arg(long)]
        totp_secret: Option<String>,
        /// 覆盖 endpoint（优先级：flag > HOSTBEE_ENDPOINT > ~/.hostbee/config.toml）
        #[arg(long)]
        endpoint: Option<String>,
    },
    /// gql 逃生门：任意 GraphQL document 原样透传到 <endpoint>/graphql
    Gql {
        /// GraphQL document（query/mutation），原样透传，不做任何改写
        document: String,
        /// 可选 variables，JSON 字符串，如 '{"id":1}'
        #[arg(long)]
        variables: Option<String>,
        /// 覆盖 endpoint（优先级：flag > HOSTBEE_ENDPOINT > ~/.hostbee/config.toml）
        #[arg(long)]
        endpoint: Option<String>,
    },
    /// refreshToken 保活 daemon：定期 refresh 轮换并原子落盘（systemd 机器自动安装
    /// user unit 并 enable，非 systemd 环境降级前台运行）
    Daemon {
        /// 轮换周期（秒），默认 86400（每天一次，远小于 refreshToken 的 7 天固定 TTL）
        #[arg(long, default_value_t = hostbee::daemon::DEFAULT_INTERVAL_SECS)]
        interval: u64,
        /// 跳过 systemd 安装，直接前台运行保活循环（unit 的 ExecStart 即此形态）
        #[arg(long)]
        foreground: bool,
        /// 覆盖 endpoint（优先级：flag > HOSTBEE_ENDPOINT > ~/.hostbee/config.toml）；
        /// 仅 --foreground 可用——systemd 服务进程读不到安装现场的 flag/env
        #[arg(long)]
        endpoint: Option<String>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        // daemon 无 stdout JSON 契约（stdout 恒空、日志全在 stderr），单独编排
        Command::Daemon {
            interval,
            foreground,
            endpoint,
        } => hostbee::daemon::main_entry(interval, foreground, endpoint.as_deref()),
        command => match run(command) {
            Ok(data) => {
                // compact 单行 JSON：效率优先，stdout 不掺杂任何其他输出。
                println!(
                    "{}",
                    serde_json::to_string(&data).expect("Value 序列化不会失败")
                );
                ExitCode::SUCCESS
            }
            Err(err) => {
                eprintln!("{}", err.stderr_json());
                ExitCode::from(FAILURE_EXIT_CODE)
            }
        },
    }
}

fn run(command: Command) -> Result<Value, CliError> {
    match command {
        Command::Login {
            contact,
            password,
            totp_secret,
            endpoint,
        } => login_cmd(contact, password, totp_secret, endpoint.as_deref()),
        Command::Gql {
            document,
            variables,
            endpoint,
        } => gql_cmd(document, variables, endpoint.as_deref()),
        // daemon 在 main() 已分流（无 stdout JSON 契约），不会进 run
        Command::Daemon { .. } => unreachable!("daemon 已在 main 分流"),
    }
}

/// 读取配置文件 + 解析 endpoint，组装会话状态（config + env 覆盖）。
fn load_session(endpoint_flag: Option<&str>) -> Result<Session, CliError> {
    let config_path = config::default_config_path();
    let file_config = match &config_path {
        Some(path) => config::read_config(path).map_err(CliError::Config)?,
        None => None,
    };
    let endpoint = endpoint::resolve_endpoint(
        endpoint_flag,
        std::env::var(ENDPOINT_ENV).ok(),
        Ok(file_config.as_ref().and_then(|c| c.endpoint.clone())),
    )?;
    Ok(Session::new(
        endpoint,
        file_config,
        config_path,
        std::env::var(REFRESH_TOKEN_ENV).ok(),
    ))
}

/// `hostbee gql`：document 原样透传，认证失败自动走 refresh/密码重登恢复。
fn gql_cmd(
    document: String,
    variables: Option<String>,
    endpoint_flag: Option<&str>,
) -> Result<Value, CliError> {
    let variables = variables.as_deref().map(gql::parse_variables).transpose()?;
    let mut session = load_session(endpoint_flag)?;
    auth::execute(
        &gql::UreqTransport::new(),
        &mut session,
        &document,
        variables,
    )
}

/// `hostbee login`：密码登录（TOTP 账号自动完成 verifyTotp 交换），
/// 凭据明文落盘，stdout 输出 AuthOutput 同构 JSON。
fn login_cmd(
    contact: Option<String>,
    password: Option<String>,
    totp_secret: Option<String>,
    endpoint_flag: Option<&str>,
) -> Result<Value, CliError> {
    let session = load_session(endpoint_flag)?;
    let contact = match from_flag_or_env(contact, CONTACT_ENV) {
        Some(value) => value,
        None => prompt_input("contact（邮箱/手机号）")?,
    };
    let password = match from_flag_or_env(password, PASSWORD_ENV) {
        Some(value) => value,
        None => prompt_input("password")?,
    };
    // 配置文件中已有的 totp_secret 兜底：重复 login 不必重传
    let totp_secret =
        from_flag_or_env(totp_secret, TOTP_SECRET_ENV).or(session.totp_secret.clone());

    let pair = auth::login_full(
        &gql::UreqTransport::new(),
        &session.endpoint,
        &contact,
        &password,
        totp_secret.as_deref(),
    )?;

    persist_login(&session, &contact, &password, &totp_secret, &pair);

    if pair.refresh_token.is_none() {
        eprintln!(
            "警告：未获取 refreshToken（账号未启用 TOTP，后端无交换路径）——\
             access token 10 分钟后过期，无法自动恢复会话"
        );
    }
    Ok(pair.to_output())
}

/// flag > env；空字符串视为未提供。
fn from_flag_or_env(flag: Option<String>, env_key: &str) -> Option<String> {
    config::non_empty(flag).or_else(|| config::non_empty(std::env::var(env_key).ok()))
}

/// 交互输入（兜底路径）：提示打到 stderr（stdout 只输出结果 JSON），
/// 从 stdin 读一行；输入为空（含非交互 EOF）报错。
fn prompt_input(label: &str) -> Result<String, CliError> {
    use std::io::{BufRead, Write};
    let mut stderr = std::io::stderr();
    let _ = stderr.write_all(format!("{label}: ").as_bytes());
    let _ = stderr.flush();
    let mut line = String::new();
    let read = std::io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| CliError::Input(format!("读取交互输入失败: {e}")))?;
    let value = line.trim();
    if read == 0 || value.is_empty() {
        return Err(CliError::Input(format!(
            "未输入 {label}：请通过 flag（--contact/--password/--totp-secret）、\
             环境变量（{CONTACT_ENV}/{PASSWORD_ENV}/{TOTP_SECRET_ENV}）或 stdin 提供"
        )));
    }
    Ok(value.to_owned())
}

/// 登录成功后明文落盘（ADR-0001：endpoint + contact + password + totp_secret + token 对）。
/// 写盘失败只警告不失败——stdout 仍输出 token，本次调用可用。
fn persist_login(
    session: &Session,
    contact: &str,
    password: &str,
    totp_secret: &Option<String>,
    pair: &auth::AuthPair,
) {
    let Some(path) = &session.config_path else {
        eprintln!("警告：拿不到 home 目录，凭据未落盘——token 仅本次调用有效");
        return;
    };
    let file_config = Config {
        endpoint: Some(session.endpoint.clone()),
        contact: Some(contact.to_owned()),
        password: Some(password.to_owned()),
        totp_secret: totp_secret.clone(),
        access_token: Some(pair.access_token.clone()),
        refresh_token: pair.refresh_token.clone(),
    };
    if let Err(e) = config::write_config_atomic(path, &file_config) {
        eprintln!("警告：{e}——凭据未落盘，token 仅本次调用有效");
    }
}
