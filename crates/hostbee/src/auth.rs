//! 认证闭环：login / refresh / verifyTotp 调用、TOTP 本地生成、
//! 以及 **401 → refresh 轮换 → 密码重登 → 重试一次** 的 token 生命周期状态机。
//!
//! 与后端（auth-flow 实测事实）对齐的关键行为：
//! - `login` 只返回 10 分钟的 login token（`refreshToken` 恒为 null）；启用 TOTP 的账号
//!   须再走一步 `verifyTotp(code)` 交换才能拿到真正的 access/refresh 对；
//! - `refresh` 同时轮换两个 token 并在服务端 revoke 旧 refreshToken——拿到新对后必须
//!   **先原子落盘再重试**原请求（写盘失败 = 会话丢失）；
//! - 认证失败**不是** HTTP 401：后端返回 HTTP 400/500 + GraphQL errors，其中
//!   `errors[*].extensions.status == 401`（code `auth.unauthorized`）才是认证失败信号；
//!   [`is_auth_failure`] 按 extensions 判定，HTTP 401（代理场景）也一并纳入。

use std::path::PathBuf;

use hmac::{Hmac, Mac};
use serde_json::{Value, json};
use sha2::Sha256;

use crate::config::Config;
use crate::error::CliError;
use crate::gql::{self, GraphqlTransport};

/// login 命令的 contact 输入来源（flag > env > 交互输入）。
pub const CONTACT_ENV: &str = "HOSTBEE_CONTACT";
/// login 命令的 password 输入来源（flag > env > 交互输入）。
pub const PASSWORD_ENV: &str = "HOSTBEE_PASSWORD";
/// login 命令的 TOTP 密钥输入来源（hex；flag > env > 配置文件已有值）。
pub const TOTP_SECRET_ENV: &str = "HOSTBEE_TOTP_SECRET";
/// refreshToken 的配置文件覆盖（spec story 3：无文件系统写入场景用）。
pub const REFRESH_TOKEN_ENV: &str = "HOSTBEE_REFRESH_TOKEN";

const LOGIN_DOCUMENT: &str = "mutation HostbeeLogin($contact: String!, $password: String!) {\n  login(contact: $contact, password: $password) {\n    accessToken\n    refreshToken\n    userInfo { id email hasTotp hasPasskey emailVerified phoneVerified callingCode phoneNumber allowTicket }\n  }\n}";

const REFRESH_DOCUMENT: &str = "mutation HostbeeRefresh($refreshToken: String) {\n  refresh(refreshToken: $refreshToken) {\n    accessToken\n    refreshToken\n    userInfo { id email hasTotp hasPasskey emailVerified phoneVerified callingCode phoneNumber allowTicket }\n  }\n}";

const VERIFY_TOTP_DOCUMENT: &str = "mutation HostbeeVerifyTotp($code: String!) {\n  verifyTotp(code: $code) {\n    accessToken\n    refreshToken\n    userInfo { id email hasTotp hasPasskey emailVerified phoneVerified callingCode phoneNumber allowTicket }\n  }\n}";

/// login / refresh / verifyTotp 成功后提取出的 token 对（AuthOutput 同构）。
#[derive(Debug, Clone)]
pub struct AuthPair {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub user_info: Option<Value>,
}

impl AuthPair {
    /// 从 `data.<field>` 提取 AuthOutput（纯函数，便于 unit 测试）。
    fn from_field(data: &Value, field: &str) -> Result<AuthPair, CliError> {
        let output = data.get(field).ok_or_else(|| {
            CliError::InvalidResponse(format!(
                "响应缺少 {field} 字段: {}",
                crate::error::truncate(&data.to_string(), 256)
            ))
        })?;
        let access_token = output
            .get("accessToken")
            .and_then(Value::as_str)
            .ok_or_else(|| CliError::InvalidResponse(format!("{field} 响应缺少 accessToken")))?
            .to_owned();
        let refresh_token = output
            .get("refreshToken")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let user_info = output.get("userInfo").filter(|v| !v.is_null()).cloned();
        Ok(AuthPair {
            access_token,
            refresh_token,
            user_info,
        })
    }

    /// 账号是否启用 TOTP（无 userInfo 时按未启用处理）。
    pub fn has_totp(&self) -> bool {
        self.user_info
            .as_ref()
            .and_then(|u| u.get("hasTotp"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }

    /// login 命令的 stdout 输出：与 GraphQL AuthOutput 同构，agent 可直接解析。
    pub fn to_output(&self) -> Value {
        json!({
            "accessToken": self.access_token,
            "refreshToken": self.refresh_token,
            "userInfo": self.user_info,
        })
    }
}

/// 一次 CLI 调用解析好的会话状态（配置文件 + 环境变量覆盖）。
///
/// - access token 只来自配置文件（spec 未定义其 env 覆盖）；
/// - refreshToken 首选 `HOSTBEE_REFRESH_TOKEN`，config 兜底；env 覆盖存在且与 config
///   不同时，config 的值作为第二候选（env 常是轮换前的旧值）；
/// - contact / password / totp_secret 只来自配置文件（密码重登凭据，ADR-0001 明文）。
#[derive(Debug, Clone)]
pub struct Session {
    pub endpoint: String,
    pub access_token: Option<String>,
    /// 首选 refreshToken（env > config）。
    pub refresh_token: Option<String>,
    /// env 覆盖存在时的第二候选：config 文件里的 refreshToken。
    pub fallback_refresh_token: Option<String>,
    pub contact: Option<String>,
    pub password: Option<String>,
    pub totp_secret: Option<String>,
    /// 配置文件路径；None（拿不到 home）时跳过持久化。
    pub config_path: Option<PathBuf>,
}

impl Session {
    pub fn new(
        endpoint: String,
        config: Option<Config>,
        config_path: Option<PathBuf>,
        env_refresh_token: Option<String>,
    ) -> Self {
        let env_refresh = crate::config::non_empty(env_refresh_token);
        let config_refresh = config.as_ref().and_then(|c| c.refresh_token.clone());
        let refresh_token = env_refresh.clone().or_else(|| config_refresh.clone());
        let fallback_refresh_token = match (&env_refresh, &config_refresh) {
            (Some(env), Some(file)) if env != file => config_refresh.clone(),
            _ => None,
        };
        Session {
            endpoint,
            access_token: config.as_ref().and_then(|c| c.access_token.clone()),
            refresh_token,
            fallback_refresh_token,
            contact: config.as_ref().and_then(|c| c.contact.clone()),
            password: config.as_ref().and_then(|c| c.password.clone()),
            totp_secret: config.as_ref().and_then(|c| c.totp_secret.clone()),
            config_path,
        }
    }
}

/// 认证失败判定（见模块文档）：`extensions.status == 401` 或真实 HTTP 401。
pub fn is_auth_failure(err: &CliError) -> bool {
    match err {
        CliError::HttpStatus { status: 401, .. } => true,
        CliError::GraphQlErrors(errors) => errors
            .iter()
            .any(|e| e.pointer("/extensions/status").and_then(Value::as_u64) == Some(401)),
        _ => false,
    }
}

/// 密码登录（单步）。返回的 AuthPair 对 TOTP 账号只是 10 分钟 login token。
pub fn login<T: GraphqlTransport>(
    transport: &T,
    endpoint: &str,
    contact: &str,
    password: &str,
) -> Result<AuthPair, CliError> {
    let body = gql::request_body(
        LOGIN_DOCUMENT,
        Some(json!({ "contact": contact, "password": password })),
    );
    post_pair(transport, endpoint, None, &body, "login")
}

/// refresh 轮换：服务端同时轮换两个 token 并 revoke 旧 refreshToken。
pub fn refresh<T: GraphqlTransport>(
    transport: &T,
    endpoint: &str,
    refresh_token: &str,
) -> Result<AuthPair, CliError> {
    let body = gql::request_body(
        REFRESH_DOCUMENT,
        Some(json!({ "refreshToken": refresh_token })),
    );
    post_pair(transport, endpoint, None, &body, "refresh")
}

/// 用 login token + TOTP 验证码交换真正的 access/refresh token 对。
pub fn verify_totp<T: GraphqlTransport>(
    transport: &T,
    endpoint: &str,
    login_token: &str,
    code: &str,
) -> Result<AuthPair, CliError> {
    let body = gql::request_body(VERIFY_TOTP_DOCUMENT, Some(json!({ "code": code })));
    post_pair(transport, endpoint, Some(login_token), &body, "verifyTotp")
}

/// 完整密码登录：login →（账号启用 TOTP 时）本地生成验证码 → verifyTotp 交换。
/// 非 TOTP 账号只得到 10 分钟 login token、没有 refreshToken（后端行为，无法绕过）。
pub fn login_full<T: GraphqlTransport>(
    transport: &T,
    endpoint: &str,
    contact: &str,
    password: &str,
    totp_secret: Option<&str>,
) -> Result<AuthPair, CliError> {
    let pair = login(transport, endpoint, contact, password)?;
    if !pair.has_totp() {
        return Ok(pair);
    }
    let secret = totp_secret.ok_or_else(|| {
        CliError::Config(
            "账号启用了 TOTP，本机没有 totp_secret：请通过 --totp-secret flag、\
             HOSTBEE_TOTP_SECRET 环境变量或配置文件提供，登录才能换取 token 对"
                .to_owned(),
        )
    })?;
    let code = generate_totp(secret).map_err(CliError::Config)?;
    verify_totp(transport, endpoint, &pair.access_token, &code)
}

/// RFC 6238 TOTP：HMAC-SHA256、30 秒步长、6 位数字（与后端 totp 配置一致；
/// ±3 步时钟容差由后端校验侧承担）。
pub fn generate_totp(secret_hex: &str) -> Result<String, String> {
    let secret = decode_hex(secret_hex)?;
    let counter = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| format!("获取当前时间失败: {e}"))?
        .as_secs()
        / 30;
    Ok(hotp_sha256(&secret, counter))
}

/// hex 字符串解码为字节（TOTP 密钥存储格式）。
fn decode_hex(text: &str) -> Result<Vec<u8>, String> {
    let text = text.trim();
    if !text.len().is_multiple_of(2) {
        return Err(format!(
            "totp_secret 长度必须为偶数（hex），实际 {}",
            text.len()
        ));
    }
    (0..text.len() / 2)
        .map(|i| {
            u8::from_str_radix(&text[i * 2..i * 2 + 2], 16)
                .map_err(|_| "totp_secret 含非法 hex 字符".to_owned())
        })
        .collect()
}

/// RFC 4226 HOTP（SHA256 变体）：动态截断取低 31 位，模 10^6 补零到 6 位。
/// offset 取**摘要最后一个字节**的低 4 位（SHA-256 摘要 32 字节，不是 SHA-1 的 20 字节）。
fn hotp_sha256(key: &[u8], counter: u64) -> String {
    let mut mac = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC 接受任意长度密钥");
    mac.update(&counter.to_be_bytes());
    let digest = mac.finalize().into_bytes();
    let offset = (digest[digest.len() - 1] & 0x0f) as usize;
    let value = ((digest[offset] & 0x7f) as u32) << 24
        | (digest[offset + 1] as u32) << 16
        | (digest[offset + 2] as u32) << 8
        | (digest[offset + 3] as u32);
    format!("{:06}", value % 1_000_000)
}

/// 执行任意 GraphQL document，内置认证失败自动恢复。
///
/// 恢复路径（上限固定，无循环）：
/// 1. 原始请求（第 1 次）未通过认证 →
/// 2. refresh 轮换（首选候选；env 覆盖时 config 候选兜底，各至多一次）→
///    成功 → **原子落盘新对** → 重试原请求一次 → 返回（无论重试成败，不再恢复）；
/// 3. refresh 全部失败且有存储的密码 → 密码重登（login + TOTP 交换）→ 落盘 → 重试一次；
/// 4. 全部失败 → 原始认证错误原样透传，恢复过程逐条附注，exit 非 0。
pub fn execute<T: GraphqlTransport>(
    transport: &T,
    session: &mut Session,
    document: &str,
    variables: Option<Value>,
) -> Result<Value, CliError> {
    let body = gql::request_body(document, variables);
    let first = send(transport, session, &body);
    if !matches!(&first, Err(err) if is_auth_failure(err)) {
        return first;
    }
    let mut notes: Vec<String> = Vec::new();

    // 恢复 1：refreshToken 轮换
    if let Some(token) = session.refresh_token.clone() {
        match refresh(transport, &session.endpoint, &token) {
            Ok(pair) => return retry_with_pair(transport, session, pair, &body),
            Err(e) => notes.push(format!("refresh 轮换未成功（{}）", summarize(&e))),
        }
        if let Some(fallback) = session.fallback_refresh_token.clone() {
            match refresh(transport, &session.endpoint, &fallback) {
                Ok(pair) => return retry_with_pair(transport, session, pair, &body),
                Err(e) => notes.push(format!("备用 refreshToken 轮换未成功（{}）", summarize(&e))),
            }
        }
    }

    // 恢复 2：用存储的密码重新 login（TOTP 账号自动完成交换）
    if let (Some(contact), Some(password)) = (session.contact.clone(), session.password.clone()) {
        match login_full(
            transport,
            &session.endpoint,
            &contact,
            &password,
            session.totp_secret.as_deref(),
        ) {
            Ok(pair) => return retry_with_pair(transport, session, pair, &body),
            Err(e) => notes.push(format!("密码重新登录未成功（{}）", summarize(&e))),
        }
    }

    Err(merged_recovery_failure(first.unwrap_err(), &notes))
}

/// 用新 token 对重试原请求（恰一次）：先落盘再重试——旧 refreshToken 已被服务端
/// revoke，不落盘就崩溃等于会话丢失。重试仍未通过认证则直接失败，不再触发恢复。
fn retry_with_pair<T: GraphqlTransport>(
    transport: &T,
    session: &mut Session,
    pair: AuthPair,
    body: &Value,
) -> Result<Value, CliError> {
    if let Err(e) = persist_pair(session, &pair) {
        eprintln!("警告：{e}——新 token 仅本次调用有效，下次调用将重新恢复会话");
    }
    session.access_token = Some(pair.access_token);
    let retry = send(transport, session, body);
    if matches!(&retry, Err(err) if is_auth_failure(err)) {
        return Err(merged_recovery_failure(
            retry.unwrap_err(),
            &["轮换/重登成功但重试原请求仍未通过认证".to_owned()],
        ));
    }
    retry
}

fn send<T: GraphqlTransport>(
    transport: &T,
    session: &Session,
    body: &Value,
) -> Result<Value, CliError> {
    let (status, text) = transport
        .post(&session.endpoint, session.access_token.as_deref(), body)
        .map_err(CliError::Transport)?;
    gql::parse_response(status, &text)
}

fn post_pair<T: GraphqlTransport>(
    transport: &T,
    endpoint: &str,
    auth: Option<&str>,
    body: &Value,
    field: &str,
) -> Result<AuthPair, CliError> {
    let (status, text) = transport
        .post(endpoint, auth, body)
        .map_err(CliError::Transport)?;
    let data = gql::parse_response(status, &text)?;
    AuthPair::from_field(&data, field)
}

/// 原子落盘新 token 对（保留 config 中已有的账号信息）。
/// 拿不到 home 目录时静默跳过（CI/容器：token 仅本次调用有效）视为成功；
/// 写盘失败返回 Err 由调用方决定日志——CLI 路径只警告不中断（调用本身已可成功，
/// 下次调用走恢复路径），daemon 路径据此计入退避。
pub(crate) fn persist_pair(session: &Session, pair: &AuthPair) -> Result<(), String> {
    let Some(path) = &session.config_path else {
        return Ok(());
    };
    let config = Config {
        endpoint: Some(session.endpoint.clone()),
        contact: session.contact.clone(),
        password: session.password.clone(),
        totp_secret: session.totp_secret.clone(),
        access_token: Some(pair.access_token.clone()),
        // 非 TOTP 账号重登拿不到新 refresh：保留旧值，不做无谓丢失
        refresh_token: pair
            .refresh_token
            .clone()
            .or_else(|| session.refresh_token.clone()),
    };
    crate::config::write_config_atomic(path, &config)
}

/// 恢复全失败时的最终错误：原始认证错误的 errors 数组**原样透传**，
/// 恢复过程逐条作为附加 errors 元素（`{"message": "..."}`），agent 可程序化区分原因。
fn merged_recovery_failure(original: CliError, notes: &[String]) -> CliError {
    if notes.is_empty() {
        return original;
    }
    let mut errors: Vec<Value> = match &original {
        CliError::GraphQlErrors(errors) => errors.clone(),
        other => vec![json!({ "message": other.message() })],
    };
    errors.extend(notes.iter().map(|note| json!({ "message": note })));
    CliError::GraphQlErrors(errors)
}

/// 错误的简短摘要（附注用）：GraphQL 错误取首条 message，其余取合成 message。
fn summarize(err: &CliError) -> String {
    let text = match err {
        CliError::GraphQlErrors(errors) => errors
            .first()
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("GraphQL errors")
            .to_owned(),
        other => other.message(),
    };
    crate::error::truncate(&text, 200)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// 本地后端实测的认证失败形状：HTTP 400 + extensions.status 401。
    const UNAUTHORIZED: &str = r#"{"data":null,"errors":[{"message":"未登录或登录已失效。","extensions":{"code":"auth.unauthorized","status":401}}]}"#;
    /// 本地后端实测的已 revoke refreshToken 复用形状：HTTP 500 messages.internal_error。
    const REVOKED_REFRESH: &str = r#"{"data":null,"errors":[{"message":"服务器内部错误，请稍后再试。","extensions":{"code":"messages.internal_error","status":500}}]}"#;

    /// 脚本化传输：按序弹出预设回复；记录每个请求的类别、auth 头与 refresh 变量。
    struct ScriptedTransport {
        replies: Mutex<VecDeque<(u16, String)>>,
        calls: Mutex<Vec<String>>,
    }

    impl ScriptedTransport {
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

    impl GraphqlTransport for ScriptedTransport {
        fn post(
            &self,
            _endpoint: &str,
            auth: Option<&str>,
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
                .push(format!("{kind}|{}|{refresh_var}", auth.unwrap_or("-")));
            let reply = self
                .replies
                .lock()
                .unwrap()
                .pop_front()
                .expect("生命周期发出了未预期的请求（回复脚本已耗尽）");
            Ok(reply)
        }
    }

    /// data.<field> 的 AuthOutput 响应体。
    fn pair_response(field: &str, access: &str, refresh: Option<&str>, has_totp: bool) -> String {
        let inner = json!({
            "accessToken": access,
            "refreshToken": refresh,
            "userInfo": {
                "id": "u1", "email": "e2e@hostbee.test", "hasTotp": has_totp,
                "hasPasskey": false, "emailVerified": true, "phoneVerified": false,
                "allowTicket": true
            }
        });
        let mut data = serde_json::Map::new();
        data.insert(field.to_owned(), inner);
        serde_json::to_string(&json!({ "data": data })).unwrap()
    }

    fn session(
        dir: &tempfile::TempDir,
        access: Option<&str>,
        refresh: Option<&str>,
        password: Option<&str>,
        totp_secret: Option<&str>,
    ) -> Session {
        let config = Config {
            endpoint: Some("http://stub".to_owned()),
            contact: Some("e2e@hostbee.test".to_owned()),
            password: password.map(str::to_owned),
            totp_secret: totp_secret.map(str::to_owned),
            access_token: access.map(str::to_owned),
            refresh_token: refresh.map(str::to_owned),
        };
        Session::new(
            "http://stub".to_owned(),
            Some(config),
            Some(dir.path().join("config.toml")),
            None,
        )
    }

    // ---------- TOTP ----------

    /// RFC 6238 Appendix B 的 SHA-256 测试向量（8 位值 mod 10^6 得 6 位）。
    #[test]
    fn hotp_与_rfc6238_sha256_向量一致() {
        let key = b"12345678901234567890123456789012";
        assert_eq!(hotp_sha256(key, 1), "119246"); // T=59
        assert_eq!(hotp_sha256(key, 37037036), "084774"); // T=1111111109
        assert_eq!(hotp_sha256(key, 37037037), "062674"); // T=1111111111
        assert_eq!(hotp_sha256(key, 41152263), "819424"); // T=1234567890
        assert_eq!(hotp_sha256(key, 66666666), "698825"); // T=2000000000
        assert_eq!(hotp_sha256(key, 666666666), "737706"); // T=20000000000
    }

    #[test]
    fn hex_解码_合法_奇数长_非法字符() {
        assert_eq!(decode_hex("6baf").unwrap(), vec![0x6b, 0xaf]);
        assert_eq!(decode_hex("").unwrap(), Vec::<u8>::new());
        assert!(decode_hex("6ba").unwrap_err().contains("偶数"));
        assert!(decode_hex("6bzz").unwrap_err().contains("hex"));
    }

    #[test]
    fn generate_totp_返回_6_位数字() {
        let code =
            generate_totp("3132333435363738393031323334353637383930313233343536373839313232")
                .unwrap();
        assert_eq!(code.len(), 6);
        assert!(code.chars().all(|c| c.is_ascii_digit()));
    }

    // ---------- 认证失败判定 ----------

    #[test]
    fn is_auth_failure_按_extensions_401_判定_不看_http_状态码() {
        // 本地后端真实形状：HTTP 400 + extensions.status 401
        let err = gql::parse_response(400, UNAUTHORIZED).unwrap_err();
        assert!(is_auth_failure(&err));
        // revoked refresh 的 500 不是认证失败（是 refresh 失败，走恢复下一候选）
        let err = gql::parse_response(500, REVOKED_REFRESH).unwrap_err();
        assert!(!is_auth_failure(&err));
        // 真实 HTTP 401（代理/网关）也纳入
        let err = gql::parse_response(401, r#"{"error":"unauthorized"}"#).unwrap_err();
        assert!(is_auth_failure(&err));
        // 其他 GraphQL 错误（如校验失败）不是认证失败
        let err = gql::parse_response(
            400,
            r#"{"data":null,"errors":[{"message":"Unknown field \"x\"."}]}"#,
        )
        .unwrap_err();
        assert!(!is_auth_failure(&err));
    }

    // ---------- Session 构造（env 覆盖优先级） ----------

    #[test]
    fn session_无_env_时_refresh_取_config_且无兜底候选() {
        let config = Config {
            refresh_token: Some("ref-file".to_owned()),
            ..Config::default()
        };
        let session = Session::new("http://s".to_owned(), Some(config), None, None);
        assert_eq!(session.refresh_token.as_deref(), Some("ref-file"));
        assert_eq!(session.fallback_refresh_token, None);
    }

    #[test]
    fn session_env_refresh_覆盖_config_且_config_值为兜底候选() {
        let config = Config {
            refresh_token: Some("ref-file".to_owned()),
            access_token: Some("acc-file".to_owned()),
            ..Config::default()
        };
        let session = Session::new(
            "http://s".to_owned(),
            Some(config),
            None,
            Some("ref-env".to_owned()),
        );
        assert_eq!(session.refresh_token.as_deref(), Some("ref-env"));
        assert_eq!(session.fallback_refresh_token.as_deref(), Some("ref-file"));
        // access token 只来自 config
        assert_eq!(session.access_token.as_deref(), Some("acc-file"));
    }

    #[test]
    fn session_env_refresh_与_config_相同值时无兜底候选() {
        let config = Config {
            refresh_token: Some("ref-same".to_owned()),
            ..Config::default()
        };
        let session = Session::new(
            "http://s".to_owned(),
            Some(config),
            None,
            Some("ref-same".to_owned()),
        );
        assert_eq!(session.refresh_token.as_deref(), Some("ref-same"));
        assert_eq!(session.fallback_refresh_token, None);
    }

    #[test]
    fn session_env_refresh_为空字符串视为未提供() {
        let config = Config {
            refresh_token: Some("ref-file".to_owned()),
            ..Config::default()
        };
        let session = Session::new(
            "http://s".to_owned(),
            Some(config),
            None,
            Some(String::new()),
        );
        assert_eq!(session.refresh_token.as_deref(), Some("ref-file"));
        assert_eq!(session.fallback_refresh_token, None);
    }

    // ---------- 生命周期状态机 ----------

    #[test]
    fn 成功路径_不触发任何恢复() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut session = session(&dir, Some("acc-ok"), Some("ref-ok"), None, None);
        let transport = ScriptedTransport::new(&[(200, r#"{"data":{"users":{"totalCount":1}}}"#)]);
        let result = execute(&transport, &mut session, "{ users { totalCount } }", None).unwrap();
        assert_eq!(result, json!({"users": {"totalCount": 1}}));
        assert_eq!(transport.calls(), vec!["gql|acc-ok|-"]);
    }

    #[test]
    fn _401_后_refresh_轮换_重试一次成功_且原子落盘() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        let mut session = session(&dir, Some("acc-old"), Some("ref-old"), Some("pw"), None);
        let transport = ScriptedTransport::new(&[
            (400, UNAUTHORIZED),
            (
                200,
                &pair_response("refresh", "acc-new", Some("ref-new"), false),
            ),
            (200, r#"{"data":{"users":{"totalCount":1}}}"#),
        ]);
        let result = execute(&transport, &mut session, "{ users { totalCount } }", None).unwrap();
        assert_eq!(result, json!({"users": {"totalCount": 1}}));
        // 原始请求恰两次：一次 401、一次用新 access token 重试成功
        assert_eq!(
            transport.calls(),
            vec!["gql|acc-old|-", "refresh|-|ref-old", "gql|acc-new|-"]
        );
        // 轮换后的新对已落盘，账号信息原样保留
        let config = crate::config::read_config(&path).unwrap().unwrap();
        assert_eq!(config.access_token.as_deref(), Some("acc-new"));
        assert_eq!(config.refresh_token.as_deref(), Some("ref-new"));
        assert_eq!(config.password.as_deref(), Some("pw"));
        assert_eq!(config.contact.as_deref(), Some("e2e@hostbee.test"));
    }

    #[test]
    fn refresh_失效_密码重登_重试一次成功() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        // 非 TOTP 账号：重登只拿到 login token，重试原请求用它（部分 resolver 接受）
        let mut session = session(&dir, Some("acc-old"), Some("ref-dead"), Some("pw"), None);
        let transport = ScriptedTransport::new(&[
            (400, UNAUTHORIZED),
            (500, REVOKED_REFRESH),
            (200, &pair_response("login", "acc-login", None, false)),
            (200, r#"{"data":{"users":{"totalCount":1}}}"#),
        ]);
        let result = execute(&transport, &mut session, "{ users { totalCount } }", None).unwrap();
        assert_eq!(result, json!({"users": {"totalCount": 1}}));
        assert_eq!(
            transport.calls(),
            vec![
                "gql|acc-old|-",
                "refresh|-|ref-dead",
                "login|-|-",
                "gql|acc-login|-"
            ]
        );
        let config = crate::config::read_config(&path).unwrap().unwrap();
        assert_eq!(config.access_token.as_deref(), Some("acc-login"));
        // 非 TOTP 重登拿不到新 refresh：旧值保留，不做无谓丢失
        assert_eq!(config.refresh_token.as_deref(), Some("ref-dead"));
    }

    #[test]
    fn refresh_失效_totp_账号自动交换后重试成功() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        let mut session = session(
            &dir,
            Some("acc-old"),
            Some("ref-dead"),
            Some("pw"),
            Some("31323334353637383930"),
        );
        let transport = ScriptedTransport::new(&[
            (400, UNAUTHORIZED),
            (500, REVOKED_REFRESH),
            (200, &pair_response("login", "acc-login", None, true)),
            (
                200,
                &pair_response("verifyTotp", "acc-full", Some("ref-full"), true),
            ),
            (200, r#"{"data":{"users":{"totalCount":1}}}"#),
        ]);
        let result = execute(&transport, &mut session, "{ users { totalCount } }", None).unwrap();
        assert_eq!(result, json!({"users": {"totalCount": 1}}));
        assert_eq!(
            transport.calls(),
            vec![
                "gql|acc-old|-",
                "refresh|-|ref-dead",
                "login|-|-",
                // verifyTotp 携带 login token 作为 HB-AUTH
                "verifyTotp|acc-login|-",
                "gql|acc-full|-"
            ]
        );
        let config = crate::config::read_config(&path).unwrap().unwrap();
        assert_eq!(config.access_token.as_deref(), Some("acc-full"));
        assert_eq!(config.refresh_token.as_deref(), Some("ref-full"));
    }

    #[test]
    fn totp_账号无本机密钥时重登失败_错误含提示() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        let mut session = session(&dir, Some("acc-old"), Some("ref-dead"), Some("pw"), None);
        let transport = ScriptedTransport::new(&[
            (400, UNAUTHORIZED),
            (500, REVOKED_REFRESH),
            (200, &pair_response("login", "acc-login", None, true)),
        ]);
        let err = execute(&transport, &mut session, "{ users { totalCount } }", None).unwrap_err();
        let json_value: Value = serde_json::from_str(&err.stderr_json()).unwrap();
        let messages: Vec<&str> = json_value["errors"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["message"].as_str())
            .collect();
        assert!(
            messages.iter().any(|m| m.contains("totp_secret")),
            "应包含 totp_secret 提示: {messages:?}"
        );
        // 恰三次调用，无 verifyTotp、无重试
        assert_eq!(transport.calls().len(), 3);
        // 失败路径不落盘（config 文件从未写入）
        assert!(!path.exists());
    }

    #[test]
    fn 全部恢复失败_原错误透传_附恢复过程注记() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut session = session(
            &dir,
            Some("acc-old"),
            Some("ref-dead"),
            Some("pw-wrong"),
            None,
        );
        let transport = ScriptedTransport::new(&[
            (400, UNAUTHORIZED),
            (500, REVOKED_REFRESH),
            (500, REVOKED_REFRESH), // 错误密码 → 后端实测同为 500 internal_error
        ]);
        let err = execute(&transport, &mut session, "{ users { totalCount } }", None).unwrap_err();
        let parsed: Value = serde_json::from_str(&err.stderr_json()).unwrap();
        let errors = parsed["errors"].as_array().unwrap();
        // 首条是原始认证错误的原样透传（含 extensions）
        assert_eq!(errors[0]["extensions"]["code"], json!("auth.unauthorized"));
        // 之后是恢复过程注记
        let messages: Vec<&str> = errors
            .iter()
            .filter_map(|e| e["message"].as_str())
            .collect();
        assert!(messages.iter().any(|m| m.contains("refresh 轮换未成功")));
        assert!(messages.iter().any(|m| m.contains("密码重新登录未成功")));
        assert_eq!(transport.calls().len(), 3);
    }

    #[test]
    fn 轮换成功_重试仍_401_不再恢复_有界() {
        let dir = tempfile::TempDir::new().unwrap();
        let mut session = session(&dir, Some("acc-old"), Some("ref-old"), Some("pw"), None);
        let transport = ScriptedTransport::new(&[
            (400, UNAUTHORIZED),
            (
                200,
                &pair_response("refresh", "acc-new", Some("ref-new"), false),
            ),
            (400, UNAUTHORIZED),
        ]);
        let err = execute(&transport, &mut session, "{ users { totalCount } }", None).unwrap_err();
        let parsed: Value = serde_json::from_str(&err.stderr_json()).unwrap();
        let messages: Vec<&str> = parsed["errors"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["message"].as_str())
            .collect();
        assert!(
            messages
                .iter()
                .any(|m| m.contains("重试原请求仍未通过认证"))
        );
        // 原始请求恰两次（初始 + 轮换后重试），不会进入密码重登
        assert_eq!(transport.calls().len(), 3);
        assert!(transport.calls().iter().all(|c| !c.starts_with("login|")));
    }

    #[test]
    fn 无任何凭据_401_原样透传_无恢复调用() {
        let session = Session::new("http://stub".to_owned(), None, None, None);
        let mut session = session;
        let transport = ScriptedTransport::new(&[(400, UNAUTHORIZED)]);
        let err = execute(&transport, &mut session, "{ users { totalCount } }", None).unwrap_err();
        let CliError::GraphQlErrors(errors) = &err else {
            panic!("应为 GraphQlErrors: {err:?}");
        };
        // 错误数组与原始完全一致，无附加注记（无恢复路径可走）
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0]["extensions"]["status"], json!(401));
        assert_eq!(transport.calls(), vec!["gql|-|-"]);
    }

    #[test]
    fn env_refresh_优先_config_兜底_两次候选按序尝试() {
        let dir = tempfile::TempDir::new().unwrap();
        let config = Config {
            endpoint: Some("http://stub".to_owned()),
            contact: Some("e2e@hostbee.test".to_owned()),
            password: Some("pw".to_owned()),
            refresh_token: Some("ref-file".to_owned()),
            access_token: Some("acc-old".to_owned()),
            ..Config::default()
        };
        let mut session = Session::new(
            "http://stub".to_owned(),
            Some(config),
            Some(dir.path().join("config.toml")),
            Some("ref-env-stale".to_owned()),
        );
        let transport = ScriptedTransport::new(&[
            (400, UNAUTHORIZED),
            (500, REVOKED_REFRESH), // env 旧值已被服务端 revoke
            (
                200,
                &pair_response("refresh", "acc-new", Some("ref-new"), false),
            ),
            (200, r#"{"data":{"users":{"totalCount":1}}}"#),
        ]);
        let result = execute(&transport, &mut session, "{ users { totalCount } }", None).unwrap();
        assert_eq!(result, json!({"users": {"totalCount": 1}}));
        assert_eq!(
            transport.calls(),
            vec![
                "gql|acc-old|-",
                "refresh|-|ref-env-stale",
                "refresh|-|ref-file",
                "gql|acc-new|-"
            ]
        );
    }

    #[test]
    fn env_refresh_无本地凭据_也能完成_并尽力落盘() {
        // CI/容器场景：只有环境变量，无任何配置文件
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("config.toml");
        let mut session = Session::new(
            "http://stub".to_owned(),
            None,
            Some(path.clone()),
            Some("ref-env".to_owned()),
        );
        let transport = ScriptedTransport::new(&[
            (400, UNAUTHORIZED), // 无 access token → 未带 HB-AUTH → 401
            (
                200,
                &pair_response("refresh", "acc-new", Some("ref-new"), false),
            ),
            (200, r#"{"data":{"users":{"totalCount":1}}}"#),
        ]);
        let result = execute(&transport, &mut session, "{ users { totalCount } }", None).unwrap();
        assert_eq!(result, json!({"users": {"totalCount": 1}}));
        assert_eq!(
            transport.calls(),
            vec!["gql|-|-", "refresh|-|ref-env", "gql|acc-new|-"]
        );
        // 新对尽力落盘（有可写 home 时），下次调用直接复用
        let config = crate::config::read_config(&path).unwrap().unwrap();
        assert_eq!(config.access_token.as_deref(), Some("acc-new"));
        assert_eq!(config.refresh_token.as_deref(), Some("ref-new"));
        assert_eq!(config.endpoint.as_deref(), Some("http://stub"));
        // 没有账号信息（无从得知），密码重登凭据为空
        assert_eq!(config.contact, None);
        assert_eq!(config.password, None);
    }

    // ---------- AuthOutput 解析 ----------

    #[test]
    fn auth_pair_解析_缺_access_token_报非法响应() {
        let data = json!({"login": {"refreshToken": "r", "userInfo": null}});
        assert!(matches!(
            AuthPair::from_field(&data, "login"),
            Err(CliError::InvalidResponse(_))
        ));
        let data = json!({"other": {}});
        assert!(matches!(
            AuthPair::from_field(&data, "login"),
            Err(CliError::InvalidResponse(_))
        ));
    }

    #[test]
    fn auth_pair_解析_user_info_null_视为缺失() {
        let data = json!({"login": {"accessToken": "a", "refreshToken": null, "userInfo": null}});
        let pair = AuthPair::from_field(&data, "login").unwrap();
        assert_eq!(pair.access_token, "a");
        assert_eq!(pair.refresh_token, None);
        assert_eq!(pair.user_info, None);
        assert!(!pair.has_totp());
        // to_output 与 AuthOutput 同构
        assert_eq!(
            pair.to_output(),
            json!({"accessToken": "a", "refreshToken": null, "userInfo": null})
        );
    }
}
