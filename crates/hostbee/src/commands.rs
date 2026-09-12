//! codegen 命令面的运行时：注册表类型、clap 命令树构建、参数→variables 提取、
//! document 选择（`--depth`/`--fields`）与执行接线。
//!
//! 数据与实现的分工：
//! - [`crate::generated`]：生成器产物（208 个 [`FieldSpec`]，覆盖 schema 全部
//!   root field，`login`/`refresh` 除外）；
//! - 本模块：消费注册表的全部运行时逻辑。
//!
//! 命令面两层结构：领域组 → field 子命令（kebab-case，Query/Mutation 混排）。
//! [`ENABLED_DOMAINS`] 为全 18 组（ticket #5 铺开完成后与注册表领域集一致，
//! 留作接线开关：registry 常驻全量，CLI 按需挂载）。

use clap::{Arg, ArgMatches, Command};
use serde_json::{Value, json};

use crate::auth::{self, CONTACT_ENV, PASSWORD_ENV, REFRESH_TOKEN_ENV, Session, TOTP_SECRET_ENV};
use crate::config::{self, Config};
use crate::endpoint::{self, ENDPOINT_ENV};
use crate::error::CliError;
use crate::generated::FIELDS;
use crate::gql;

/// selection set 深度档位上限（0..=8，与生成器档位一致）。
pub const MAX_DEPTH: i64 = 8;
/// 默认深度档（产物 `documents[DEFAULT_DEPTH]` 即默认 document）。
pub const DEFAULT_DEPTH: usize = 3;

/// 已接线领域组：按领域名在 [`crate::generated::FIELDS`] 中筛选挂进 CLI。
///
/// ticket #5 起全 18 组开启（顺序与 codegen 的 `DOMAIN_ORDER`/survey §2 一致，
/// 即 `--help` 的组序）。schema 新增领域时生成器会 fail-fast 要求补分组规则，
/// 此处同步增补一行域名即完成接线。
pub const ENABLED_DOMAINS: &[&str] = &[
    "auth",
    "user",
    "wallet",
    "vm",
    "infra",
    "store",
    "order",
    "subscription",
    "payment",
    "kyc",
    "ticket",
    "notice",
    "plugin",
    "task",
    "sms",
    "settings",
    "accesslog",
    "admin",
];

/// 手写别名（不进 codegen 规则，见 codegen-design.md §2.4；逐条注释指向生成名，
/// 避免 codegen 腐烂）。clap 侧 `visible_alias`：`vm list` → `vm vm-instances`。
const ALIASES: &[(&str, &str, &str)] = &[
    // (领域, 别名, 生成的命令名)
    ("vm", "list", "vm-instances"), // vmInstances：分页实例列表
    ("vm", "search", "vm-instance-search"), // vmInstanceSearch：实例搜索
    // orders-paging：分页订单列表。codegen-design.md §2.4 原指全量查询 orders，
    // 但 live 后端 6611 单在 depth 3 下全量返回超传输上限（10MB，实测报错）；
    // 别名改指分页变体，与「默认单页」哲学一致（全量场景用 orders --depth 0）。
    ("order", "list", "orders-paging"),
];

/// 参数 → flag 的种类。
#[derive(Debug, Clone)]
pub enum ArgKind {
    /// `Int` 标量。
    Int,
    /// `Float` 标量。
    Float,
    /// `Boolean` 标量。
    Boolean,
    /// `String` / `ID` / `DateTime` / `Upload` 等字符串标量。
    Str,
    /// enum 标量：值为 schema 枚举名（help 自动列举合法值）。
    Enum(&'static [&'static str]),
    /// JSON 透传：输入对象（含 oneOf）/ `JSON` / `Money` / `BorshOrJson`（json 分支）
    /// / 输入对象列表。整个 flag 值按 JSON 解析后进 variables。
    Json,
    /// `[Int!]` 标量列表（逗号分隔或重复传值）。
    IntList,
    /// `[String!]` 列表。
    StrList,
}

/// 一个 GraphQL 参数的 flag 规格（生成器产物的最小单元）。
pub struct ArgSpec {
    /// flag 名（kebab-case，不含 `--`）。
    pub flag: &'static str,
    /// GraphQL 参数名（variables 键）。
    pub arg: &'static str,
    pub kind: ArgKind,
    /// GraphQL 侧必填（非 null 输入位置）。
    pub required: bool,
    /// clap default_value 文本（分页默认 / 必填输入对象的空 JSON）。
    pub default: Option<&'static str>,
    /// 帮助文本（schema docstring + 类型/透传提示）。
    pub help: &'static str,
}

/// 一个 root field 的命令规格（生成器产物单元）。
pub struct FieldSpec {
    /// 领域组名（schema-survey.md §1 分组规则）。
    pub domain: &'static str,
    /// kebab-case 子命令名。
    pub command: &'static str,
    /// GraphQL root field 名。
    pub field: &'static str,
    /// MutationRoot 下的 field 为 true。
    pub mutation: bool,
    /// 返回值是否对象类型（决定 `--depth`/`--fields` 是否挂载）。
    pub returns_object: bool,
    /// operation 头 + field + 实参（`--fields` 覆盖 selection set 时拼接）。
    pub prefix: &'static str,
    /// depth 0..=8 每档一份完整 document（默认档位 [`DEFAULT_DEPTH`]）。
    pub documents: &'static [&'static str],
    /// 子命令 about（schema docstring 优先，缺省为 GraphQL 签名）。
    pub about: &'static str,
    pub args: &'static [ArgSpec],
}

// ========== CLI 构建 ==========

/// 构建完整命令树：手写命令（login/gql）+ 已接线领域组。
pub fn build_cli() -> Command {
    let root = Command::new("hostbee")
        .version(env!("CARGO_PKG_VERSION"))
        .about("rustybee 后端的 agent 专用 CLI：命令面由 schema codegen 按领域生成，stdout 只输出结果 JSON")
        .disable_help_subcommand(true)
        .arg_required_else_help(true)
        .subcommand(login_command())
        .subcommand(gql_command())
        .subcommand(daemon_command());
    install_generated(root)
}

/// `hostbee login`（ticket #3）：flag 语义与原 derive 定义完全一致。
fn login_command() -> Command {
    Command::new("login")
        .about("交互式登录：凭据明文持久化到 ~/.hostbee/config.toml（ADR-0001）")
        .arg(
            Arg::new("contact")
                .long("contact")
                .value_name("CONTACT")
                .help("联系方式（邮箱/手机号）；缺省时按 flag > HOSTBEE_CONTACT > 交互输入解析"),
        )
        .arg(
            Arg::new("password")
                .long("password")
                .value_name("PASSWORD")
                .help("密码；缺省时按 flag > HOSTBEE_PASSWORD > 交互输入解析"),
        )
        .arg(
            Arg::new("totp-secret")
                .long("totp-secret")
                .value_name("HEX")
                .help(
                    "TOTP 密钥（hex）；TOTP 账号登录换取 token 对必需，缺省时按 flag > \
                       HOSTBEE_TOTP_SECRET > 配置文件已有值解析",
                ),
        )
        .arg(
            Arg::new("endpoint")
                .long("endpoint")
                .value_name("URL")
                .help("覆盖 endpoint（优先级：flag > HOSTBEE_ENDPOINT > ~/.hostbee/config.toml）"),
        )
}

/// `hostbee gql`（ticket #2）：document 原样透传的逃生门。
fn gql_command() -> Command {
    Command::new("gql")
        .about("gql 逃生门：任意 GraphQL document 原样透传到 <endpoint>/graphql")
        .arg(
            Arg::new("document")
                .value_name("DOCUMENT")
                .help("GraphQL document（query/mutation），原样透传，不做任何改写")
                .required(true),
        )
        .arg(
            Arg::new("variables")
                .long("variables")
                .value_name("JSON")
                .help("可选 variables，JSON 字符串，如 '{\"id\":1}'"),
        )
        .arg(
            Arg::new("endpoint")
                .long("endpoint")
                .value_name("URL")
                .help("覆盖 endpoint（优先级：flag > HOSTBEE_ENDPOINT > ~/.hostbee/config.toml）"),
        )
}

/// 把 [`ENABLED_DOMAINS`] 的领域组挂进根命令。
fn install_generated(mut root: Command) -> Command {
    for domain in ENABLED_DOMAINS {
        root = root.subcommand(domain_command(domain));
    }
    root
}

/// `hostbee daemon`（ticket #6）：refreshToken 保活。无 stdout JSON 契约，
/// 由 main 直接分流到 [`crate::daemon::main_entry`]（stdout 恒空、日志全在 stderr）。
fn daemon_command() -> Command {
    Command::new("daemon")
        .about(
            "refreshToken 保活 daemon：定期 refresh 轮换并原子落盘（systemd 机器自动安装 \
             user unit 并 enable，非 systemd 环境降级前台运行）",
        )
        .arg(
            Arg::new("interval")
                .long("interval")
                .value_name("SECS")
                .help("轮换周期（秒），默认 86400（每天一次，远小于 refreshToken 的 7 天固定 TTL）")
                .value_parser(clap::value_parser!(u64))
                .default_value(crate::daemon::DEFAULT_INTERVAL_SECS.to_string()),
        )
        .arg(
            Arg::new("foreground")
                .long("foreground")
                .help("跳过 systemd 安装，直接前台运行保活循环（unit 的 ExecStart 即此形态）")
                .action(clap::ArgAction::SetTrue),
        )
        .arg(
            Arg::new("endpoint")
                .long("endpoint")
                .value_name("URL")
                .help(
                    "覆盖 endpoint（优先级：flag > HOSTBEE_ENDPOINT > ~/.hostbee/config.toml）；\
                     仅 --foreground 可用——systemd 服务进程读不到安装现场的 flag/env",
                ),
        )
}

/// 构建一个领域组子命令（组内全部 field 子命令）。
fn domain_command(domain: &str) -> Command {
    let fields: Vec<&FieldSpec> = FIELDS.iter().filter(|f| f.domain == domain).collect();
    let mut cmd = Command::new(domain.to_owned()).about(format!(
        "{domain} 领域命令组（{} 个，Query/Mutation 混排）",
        fields.len()
    ));
    for spec in fields {
        cmd = cmd.subcommand(field_command(spec));
    }
    cmd
}

/// 构建一个 field 子命令：参数 flags + 运行时 flags（--depth/--fields/--endpoint）。
fn field_command(spec: &FieldSpec) -> Command {
    let mut cmd = Command::new(spec.command).about(spec.about);
    for (domain, alias, target) in ALIASES {
        if *domain == spec.domain && *target == spec.command {
            cmd = cmd.visible_alias(alias);
        }
    }
    for arg in spec.args {
        cmd = cmd.arg(build_arg(arg));
    }
    if spec.returns_object {
        cmd = cmd
            .arg(
                Arg::new("depth")
                    .long("depth")
                    .value_name("N")
                    .value_parser(0..=MAX_DEPTH)
                    .default_value(DEFAULT_DEPTH.to_string())
                    .help("selection set 展开深度（0..=8，默认 3；标量不消耗深度，环切到 { id }）"),
            )
            .arg(
                Arg::new("fields")
                    .long("fields")
                    .value_name("SELECTION")
                    .help("覆盖生成的 selection set（形如 '{ nodes { id status } totalNum }'）"),
            );
    }
    cmd.arg(
        Arg::new("endpoint")
            .long("endpoint")
            .value_name("URL")
            .help("覆盖 endpoint（优先级：flag > HOSTBEE_ENDPOINT > ~/.hostbee/config.toml）"),
    )
}

/// 单个参数 → clap Arg。
fn build_arg(spec: &ArgSpec) -> Arg {
    let mut arg = Arg::new(spec.flag)
        .long(spec.flag)
        .value_name(screaming(spec.flag))
        .help(spec.help);
    match spec.kind {
        ArgKind::Int => arg = arg.value_parser(clap::value_parser!(i64)),
        ArgKind::Float => arg = arg.value_parser(clap::value_parser!(f64)),
        ArgKind::Boolean => arg = arg.value_parser(clap::value_parser!(bool)),
        ArgKind::Str => arg = arg.value_parser(clap::value_parser!(String)),
        ArgKind::Json => arg = arg.value_parser(clap::value_parser!(String)),
        ArgKind::IntList => {
            arg = arg
                .value_parser(clap::value_parser!(i64))
                .value_delimiter(',')
                .num_args(1..)
                .action(clap::ArgAction::Append)
        }
        ArgKind::StrList => {
            arg = arg
                .value_parser(clap::value_parser!(String))
                .value_delimiter(',')
                .num_args(1..)
                .action(clap::ArgAction::Append)
        }
        ArgKind::Enum(values) => {
            arg = arg.value_parser(clap::builder::PossibleValuesParser::new(
                values.iter().copied(),
            ))
        }
    }
    // GraphQL 必填 → clap 必填；带默认值（分页/空 JSON）的必填参数由默认值满足。
    match spec.default {
        Some(default) => arg.default_value(default),
        None if spec.required => arg.required(true),
        None => arg,
    }
}

/// kebab-case flag 名 → SCREAMING_SNAKE 值名（`page-size` → `PAGE_SIZE`）。
fn screaming(flag: &str) -> String {
    flag.replace('-', "_").to_uppercase()
}

/// 领域组 → 命令名 → 注册表条目。
fn find(domain: &str, command: &str) -> Option<&'static FieldSpec> {
    FIELDS
        .iter()
        .find(|f| f.domain == domain && f.command == command)
}

// ========== 运行时 ==========

/// 分发一个领域组子命令：定位注册表条目并执行。
pub fn dispatch_generated(domain: &str, matches: &ArgMatches) -> Result<Value, CliError> {
    let Some((command, sub)) = matches.subcommand() else {
        return Err(CliError::Input(format!(
            "领域组 {domain} 需要子命令；`hostbee {domain} --help` 查看全部"
        )));
    };
    let spec = find(domain, command).ok_or_else(|| {
        CliError::Input(format!(
            "领域 {domain} 中没有命令 {command}（注册表未命中）"
        ))
    })?;
    execute_spec(spec, sub)
}

/// 执行一个生成的命令：document 选择 + variables 提取 + 认证生命周期。
pub fn execute_spec(spec: &FieldSpec, matches: &ArgMatches) -> Result<Value, CliError> {
    let endpoint_flag = matches.get_one::<String>("endpoint").map(String::as_str);
    let document = document(spec, matches)?;
    let variables = variables(spec, matches)?;
    let mut session = load_session(endpoint_flag)?;
    auth::execute(
        &gql::UreqTransport::new(),
        &mut session,
        &document,
        Some(variables),
    )
}

/// 按 `--depth`/`--fields` 选出本次执行的 document。
///
/// 标量返回的命令不挂载 `--depth`/`--fields`（无 selection set，九档同文），
/// 直接取默认档。
pub fn document(spec: &FieldSpec, matches: &ArgMatches) -> Result<String, CliError> {
    if !spec.returns_object {
        return Ok((*spec
            .documents
            .get(DEFAULT_DEPTH)
            .expect("documents 恒为 0..=8 九档"))
        .to_owned());
    }
    if let Some(selection) = matches.get_one::<String>("fields") {
        let selection = selection.trim();
        if !(selection.starts_with('{') && selection.ends_with('}')) {
            return Err(CliError::Input(format!(
                "--fields 需要完整 selection set（{{ 开头、}} 结尾），形如 '{{ nodes {{ id }} totalNum }}'，\
                 实际: {selection}"
            )));
        }
        return Ok(format!("{} {selection} }}", spec.prefix));
    }
    let depth = matches
        .get_one::<i64>("depth")
        .copied()
        .unwrap_or(DEFAULT_DEPTH as i64);
    if let Some(doc) = spec.documents.get(depth as usize) {
        return Ok((*doc).to_owned());
    }
    // 不可达（depth 有 range 校验、documents 恒为 9 档）；防御性兜底保持全函数。
    Ok(format!("{} }}", spec.prefix))
}

/// 提取 flags → variables 对象（缺省的可选参数直接缺省，不发 null）。
pub fn variables(spec: &FieldSpec, matches: &ArgMatches) -> Result<Value, CliError> {
    let mut vars = serde_json::Map::new();
    for arg in spec.args {
        let key = arg.arg;
        match &arg.kind {
            ArgKind::Int => {
                if let Some(v) = matches.get_one::<i64>(arg.flag) {
                    vars.insert(key.to_owned(), json!(v));
                }
            }
            ArgKind::Float => {
                if let Some(v) = matches.get_one::<f64>(arg.flag) {
                    vars.insert(key.to_owned(), json!(v));
                }
            }
            ArgKind::Boolean => {
                if let Some(v) = matches.get_one::<bool>(arg.flag) {
                    vars.insert(key.to_owned(), json!(v));
                }
            }
            ArgKind::Str | ArgKind::Enum(_) | ArgKind::Json => {
                let Some(raw) = matches.get_one::<String>(arg.flag) else {
                    continue;
                };
                if matches!(arg.kind, ArgKind::Json) {
                    let value: Value = serde_json::from_str(raw).map_err(|e| {
                        CliError::Input(format!("--{} 不是合法 JSON: {e}", arg.flag))
                    })?;
                    vars.insert(key.to_owned(), value);
                } else {
                    vars.insert(key.to_owned(), json!(raw));
                }
            }
            ArgKind::IntList => {
                if let Some(values) = matches.get_many::<i64>(arg.flag) {
                    let list: Vec<Value> = values.map(|v| json!(v)).collect();
                    vars.insert(key.to_owned(), Value::Array(list));
                }
            }
            ArgKind::StrList => {
                if let Some(values) = matches.get_many::<String>(arg.flag) {
                    let list: Vec<Value> = values.map(|v| json!(v)).collect();
                    vars.insert(key.to_owned(), Value::Array(list));
                }
            }
        }
    }
    Ok(Value::Object(vars))
}

// ========== 手写命令（login/gql，自 main.rs 迁移） ==========

/// 读取配置文件 + 解析 endpoint，组装会话状态（config + env 覆盖）。
pub(crate) fn load_session(endpoint_flag: Option<&str>) -> Result<Session, CliError> {
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
pub fn gql_cmd(matches: &ArgMatches) -> Result<Value, CliError> {
    let document = matches
        .get_one::<String>("document")
        .cloned()
        .unwrap_or_default();
    let variables = matches
        .get_one::<String>("variables")
        .map(|raw| gql::parse_variables(raw))
        .transpose()?;
    let mut session = load_session(matches.get_one::<String>("endpoint").map(String::as_str))?;
    auth::execute(
        &gql::UreqTransport::new(),
        &mut session,
        &document,
        variables,
    )
}

/// `hostbee login`：密码登录（TOTP 账号自动完成 verifyTotp 交换），
/// 凭据明文落盘，stdout 输出 AuthOutput 同构 JSON。
pub fn login_cmd(matches: &ArgMatches) -> Result<Value, CliError> {
    let session = load_session(matches.get_one::<String>("endpoint").map(String::as_str))?;
    let contact = match from_flag_or_env(
        matches.get_one::<String>("contact").map(String::as_str),
        CONTACT_ENV,
    ) {
        Some(value) => value,
        None => prompt_input("contact（邮箱/手机号）")?,
    };
    let password = match from_flag_or_env(
        matches.get_one::<String>("password").map(String::as_str),
        PASSWORD_ENV,
    ) {
        Some(value) => value,
        None => prompt_input("password")?,
    };
    // 配置文件中已有的 totp_secret 兜底：重复 login 不必重传
    let totp_secret = from_flag_or_env(
        matches.get_one::<String>("totp-secret").map(String::as_str),
        TOTP_SECRET_ENV,
    )
    .or(session.totp_secret.clone());

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
fn from_flag_or_env(flag: Option<&str>, env_key: &str) -> Option<String> {
    config::non_empty(flag.map(str::to_owned))
        .or_else(|| config::non_empty(std::env::var(env_key).ok()))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 真实注册表条目。
    fn spec(command: &str) -> &'static FieldSpec {
        FIELDS
            .iter()
            .find(|f| f.domain == "vm" && f.command == command)
            .unwrap_or_else(|| panic!("注册表应有 vm/{command}"))
    }

    /// 用注册表条目构建单命令树并解析参数（只挂这一个叶子）。
    fn leaf_matches(spec: &FieldSpec, args: &[&str]) -> ArgMatches {
        let cmd = Command::new("t").subcommand(field_command(spec));
        let matches = cmd
            .try_get_matches_from(std::iter::once("t").chain(args.iter().copied()))
            .expect("参数应可解析");
        matches.subcommand().expect("应命中叶子子命令").1.clone()
    }

    #[test]
    fn 分页默认值_必填输入对象_空_json_默认() {
        let spec = spec("vm-instances");
        let m = leaf_matches(spec, &["vm-instances"]);
        let vars = variables(spec, &m).unwrap();
        assert_eq!(vars, json!({"pageSize": 25, "pageNum": 1, "filter": {}}));
    }

    #[test]
    fn 分页与_json_透传_显式值() {
        let spec = spec("vm-instances");
        let m = leaf_matches(
            spec,
            &[
                "vm-instances",
                "--page-size",
                "1",
                "--page-num",
                "2",
                "--filter",
                r#"{"hostname":"vm-1"}"#,
            ],
        );
        let vars = variables(spec, &m).unwrap();
        assert_eq!(
            vars,
            json!({"pageSize": 1, "pageNum": 2, "filter": {"hostname": "vm-1"}})
        );
    }

    #[test]
    fn 可选参数_缺省时不进_variables() {
        // vm-instances-connection 的 after/before/last/filter 与 first（有默认）不同：
        // after 缺省 → variables 无 after 键；first 缺省 → 默认 25。
        let spec = spec("vm-instances-connection");
        let m = leaf_matches(spec, &["vm-instances-connection"]);
        let vars = variables(spec, &m).unwrap();
        assert_eq!(vars.get("first"), Some(&json!(25)));
        assert_eq!(vars.get("after"), None);
        assert_eq!(vars.get("filter"), None);
    }

    #[test]
    fn json_非法时_输入错误() {
        let spec = spec("vm-instances");
        let m = leaf_matches(spec, &["vm-instances", "--filter", "{bad"]);
        let err = variables(spec, &m).unwrap_err();
        assert!(err.stderr_json().contains("--filter"));
    }

    #[test]
    fn 列表标量_逗号分隔与多值() {
        let spec = FIELDS
            .iter()
            .find(|f| f.field == "hypervisorsByAzIds")
            .expect("注册表应有 hypervisorsByAzIds");
        let m = leaf_matches(
            spec,
            &["hypervisors-by-az-ids", "--az-ids", "1,2", "--az-ids", "3"],
        );
        let vars = variables(spec, &m).unwrap();
        assert_eq!(vars.get("azIds"), Some(&json!([1, 2, 3])));
    }

    #[test]
    fn enum_非法值被_clap_拒绝() {
        let spec = spec("vm-power-action");
        let cmd = Command::new("t").subcommand(field_command(spec));
        assert!(
            cmd.try_get_matches_from(["t", "vm-power-action", "--vm-id", "1", "--action", "NOPE"])
                .is_err()
        );
    }

    #[test]
    fn 默认_document_为_depth_3_档() {
        let spec = spec("vm-instances");
        let m = leaf_matches(spec, &["vm-instances"]);
        assert_eq!(document(spec, &m).unwrap(), spec.documents[3]);
    }

    #[test]
    fn depth_选档_与_fields_覆盖() {
        let spec = spec("vm-instances");
        let m = leaf_matches(spec, &["vm-instances", "--depth", "0"]);
        assert_eq!(document(spec, &m).unwrap(), spec.documents[0]);
        let m = leaf_matches(
            spec,
            &[
                "vm-instances",
                "--fields",
                "{ nodes { id status } totalNum }",
            ],
        );
        assert_eq!(
            document(spec, &m).unwrap(),
            format!("{} {{ nodes {{ id status }} totalNum }} }}", spec.prefix)
        );
    }

    #[test]
    fn fields_不完整_报输入错误() {
        let spec = spec("vm-instances");
        let m = leaf_matches(spec, &["vm-instances", "--fields", "nodes { id }"]);
        assert!(matches!(document(spec, &m), Err(CliError::Input(_))));
    }

    #[test]
    fn 标量返回命令_无_depth_与_fields_flag() {
        // updateVmInstance 返回 String!：--fields 不是合法 flag（clap 拒绝）。
        let spec = spec("update-vm-instance");
        let cmd = Command::new("t").subcommand(field_command(spec));
        assert!(
            cmd.try_get_matches_from([
                "t",
                "update-vm-instance",
                "--input",
                "{}",
                "--fields",
                "{ x }"
            ])
            .is_err()
        );
        // 但 document 直接可用（九档同文）。
        let m = leaf_matches(spec, &["update-vm-instance", "--input", r#"{"id":1}"#]);
        let vars = variables(spec, &m).unwrap();
        assert_eq!(vars, json!({"input": {"id": 1}}));
        assert_eq!(document(spec, &m).unwrap(), spec.documents[3]);
    }

    #[test]
    fn 别名_解析到生成命令() {
        // vm list → vm-instances；vm search → vm-instance-search
        let matches = domain_command("vm")
            .try_get_matches_from(["vm", "list"])
            .unwrap();
        assert_eq!(matches.subcommand().unwrap().0, "vm-instances");
        let matches = domain_command("vm")
            .try_get_matches_from(["vm", "search", "--search-term", "web-1"])
            .unwrap();
        assert_eq!(matches.subcommand().unwrap().0, "vm-instance-search");
    }

    #[test]
    fn build_cli_挂载全_18_领域_全量命令面() {
        let cmd = build_cli();
        // 根命令：3 个手写（login/gql/daemon）+ 18 个领域组
        let subcommands: Vec<&str> = cmd.get_subcommands().map(|s| s.get_name()).collect();
        let mut expected: Vec<&str> = ["login", "gql", "daemon"].to_vec();
        expected.extend(ENABLED_DOMAINS);
        assert_eq!(subcommands, expected, "根命令面应为手写命令 + 全部领域组");
        // 每个领域组挂载注册表中该域的全部命令（208 个生成命令逐组可命中）
        let mut total = 0;
        for domain in ENABLED_DOMAINS {
            let group = cmd
                .find_subcommand(domain)
                .unwrap_or_else(|| panic!("领域组 {domain} 应已挂载"));
            let wired: std::collections::BTreeSet<&str> =
                group.get_subcommands().map(|s| s.get_name()).collect();
            let registered: std::collections::BTreeSet<&str> = FIELDS
                .iter()
                .filter(|f| f.domain == *domain)
                .map(|f| f.command)
                .collect();
            assert_eq!(wired, registered, "领域 {domain} 的挂载命令应与注册表一致");
            total += registered.len();
        }
        assert_eq!(total, FIELDS.len());
        assert_eq!(total, 208, "全 schema 铺开：208 个生成命令全部可见");
        // 手写别名逐条可见且指向注册表中的生成命令
        for (domain, alias, target) in ALIASES {
            let group = cmd.find_subcommand(domain).expect("别名所在领域应挂载");
            let leaf = group
                .find_subcommand(target)
                .unwrap_or_else(|| panic!("别名目标 {domain}/{target} 应在注册表"));
            assert!(
                leaf.get_visible_aliases().any(|a| a == *alias),
                "{domain} 组的 {alias} 别名应可见"
            );
        }
        // 手写命令仍在；auth 组不含 login/refresh（ticket #3/#4 手写让位）
        assert!(
            build_cli()
                .try_get_matches_from(["hostbee", "gql", "{ x }"])
                .is_ok()
        );
        assert!(
            build_cli()
                .try_get_matches_from(["hostbee", "auth", "login"])
                .is_err()
        );
    }

    #[test]
    fn 命令注册表_无flag冲突与运行时flag撞名() {
        // 每个命令的 flags 互不重复，且不与运行时 flag（--depth/--fields/--endpoint）撞名；
        // schema 演进引入撞名时在产物断言层拦住（生成器 fail-fast 优于 clap 运行期怪象）。
        for spec in FIELDS {
            let mut seen = std::collections::BTreeSet::new();
            for arg in spec.args {
                assert!(
                    seen.insert(arg.flag),
                    "{} 的 flag --{} 重复",
                    spec.command,
                    arg.flag
                );
                assert!(
                    !matches!(arg.flag, "depth" | "fields" | "endpoint"),
                    "{} 的参数 flag --{} 与运行时 flag 撞名",
                    spec.command,
                    arg.flag
                );
            }
        }
    }
}
