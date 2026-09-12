//! 内存 stub GraphQL HTTP server（tiny_http），e2e harness 的本地依赖。
//!
//! 三种模式：
//! - [`GraphqlStub::canned`]：固定返回给定状态码 + body（模拟成功、GraphQL errors、非 2xx 等）；
//! - [`GraphqlStub::echo`]：把收到的 query / variables 原样回填到 `data`，用于断言透传；
//! - [`GraphqlStub::auth`]：模拟真实后端的认证语义——HB-AUTH 校验、401 形状
//!   （HTTP 400 + `extensions.status=401`，后端不发 HTTP 401）、`login` 只发 login token、
//!   `verifyTotp` 交换 token 对、`refresh` 轮换（旧 refreshToken 即废）、
//!   错误密码 / 旧 refreshToken 复用 → 500 `messages.internal_error`。
//!   附带故障开关 [`GraphqlStub::set_outage`]：开启时 login/verifyTotp/refresh 一律
//!   500（模拟后端不可用，驱动 daemon 退避与恢复路径）。
//!
//! 同时捕获每个请求的 Content-Type、HB-AUTH 头与 body，供测试断言请求形状。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{Value, json};

/// stub 捕获到的一次请求。
#[derive(Clone, Debug)]
pub struct CapturedRequest {
    pub content_type: Option<String>,
    /// HB-AUTH 头（去掉 `Bearer ` 前缀后的裸值）；未携带为 None。
    pub hb_auth: Option<String>,
    pub body: String,
}

enum Mode {
    /// 固定响应。
    Canned { status: u16, body: String },
    /// 回显 query / variables。
    Echo,
    /// 有状态认证模拟（真实后端语义）。
    Auth {
        config: AuthStubConfig,
        state: Arc<Mutex<AuthState>>,
        /// 认证 mutation 故障开关：true 时 login/verifyTotp/refresh 一律 500。
        outage: Arc<AtomicBool>,
    },
}

/// 有状态认证 stub 的账号配置。
#[derive(Clone)]
pub struct AuthStubConfig {
    pub contact: String,
    pub password: String,
    /// true 时 login 返回 hasTotp=true 的 login token，须经 verifyTotp 交换才有 token 对。
    pub has_totp: bool,
}

/// 有状态认证 stub 的服务端状态（测试可读取断言）。
#[derive(Clone, Debug)]
pub struct AuthState {
    /// 最新 login token（verifyTotp 的合法 HB-AUTH）。
    pub login_token: Option<String>,
    /// 历次签发的 access token（模拟后端：access token 独立有效期，不随轮换撤销）。
    pub access_tokens: Vec<String>,
    /// 当前唯一有效的 refreshToken（每次 refresh / verifyTotp 交换即轮换）。
    pub refresh_token: String,
    pub login_count: u32,
    pub refresh_count: u32,
    pub totp_count: u32,
}

pub struct GraphqlStub {
    url: String,
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
    auth_state: Option<Arc<Mutex<AuthState>>>,
    outage: Option<Arc<AtomicBool>>,
}

impl GraphqlStub {
    /// 固定响应的 stub。
    pub fn canned(status: u16, body: impl Into<String>) -> Self {
        Self::start(Mode::Canned {
            status,
            body: body.into(),
        })
    }

    /// 回显模式的 stub：`data` = 收到的 `{query, variables}`。
    pub fn echo() -> Self {
        Self::start(Mode::Echo)
    }

    /// 有状态认证 stub。初始 refreshToken 为 [`AuthStubConfig::initial_refresh`]
    /// （测试用它预置配置文件里的 refresh_token）。
    pub fn auth(config: AuthStubConfig) -> Self {
        let state = Arc::new(Mutex::new(AuthState {
            login_token: None,
            access_tokens: Vec::new(),
            refresh_token: AuthStubConfig::initial_refresh().to_owned(),
            login_count: 0,
            refresh_count: 0,
            totp_count: 0,
        }));
        let outage = Arc::new(AtomicBool::new(false));
        Self::start(Mode::Auth {
            config,
            state,
            outage,
        })
    }

    /// stub 的 base URL（传给 hostbee 的 endpoint）。
    pub fn url(&self) -> &str {
        &self.url
    }

    /// 已捕获的请求（按到达顺序）。
    pub fn requests(&self) -> Vec<CapturedRequest> {
        self.requests.lock().unwrap().clone()
    }

    /// 有状态认证 stub 的当前服务端状态（非认证模式返回 None）。
    pub fn auth_state(&self) -> Option<AuthState> {
        self.auth_state.as_ref().map(|s| s.lock().unwrap().clone())
    }

    /// 认证后端故障开关（仅 auth 模式）：true = login/verifyTotp/refresh 一律 500，
    /// false = 恢复正常。驱动 daemon 的退避与恢复路径。
    pub fn set_outage(&self, down: bool) {
        if let Some(outage) = &self.outage {
            outage.store(down, Ordering::SeqCst);
        }
    }

    fn start(mode: Mode) -> Self {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("stub server 绑定失败");
        let url = format!("http://{}", server.server_addr());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let shared = requests.clone();
        let auth_state = match &mode {
            Mode::Auth { state, .. } => Some(state.clone()),
            _ => None,
        };
        let outage = match &mode {
            Mode::Auth { outage, .. } => Some(outage.clone()),
            _ => None,
        };
        thread::spawn(move || {
            for mut request in server.incoming_requests() {
                // 先捕获请求再响应：客户端拿到响应时，捕获必然已可见。
                let content_type = request
                    .headers()
                    .iter()
                    .find(|h| h.field.equiv("Content-Type"))
                    .map(|h| h.value.as_str().to_owned());
                let hb_auth = request
                    .headers()
                    .iter()
                    .find(|h| h.field.equiv("HB-AUTH"))
                    .and_then(|h| h.value.as_str().strip_prefix("Bearer ").map(str::to_owned));
                let mut body = String::new();
                let _ = request.as_reader().read_to_string(&mut body);
                shared.lock().unwrap().push(CapturedRequest {
                    content_type,
                    hb_auth: hb_auth.clone(),
                    body: body.clone(),
                });
                let (status, response_body) = respond(&mode, &body, hb_auth.as_deref());
                let _ = request.respond(
                    tiny_http::Response::from_string(response_body).with_status_code(status),
                );
            }
        });
        GraphqlStub {
            url,
            requests,
            auth_state,
            outage,
        }
    }
}

/// 认证 stub 的响应体形状（对齐真实后端实测，见 spec-notes/auth-flow.md）。
mod shape {
    use serde_json::json;

    /// 受保护字段未授权：HTTP 400 + extensions.status 401（后端不发 HTTP 401）。
    pub fn unauthorized() -> String {
        json!({
            "data": null,
            "errors": [{
                "message": "未登录或登录已失效。",
                "extensions": { "code": "auth.unauthorized", "status": 401 }
            }]
        })
        .to_string()
    }

    /// 错误密码 / 已 revoke 的 refreshToken 复用：HTTP 500 internal_error。
    pub fn internal_error() -> String {
        json!({
            "data": null,
            "errors": [{
                "message": "服务器内部错误，请稍后再试。",
                "extensions": { "code": "messages.internal_error", "status": 500 }
            }]
        })
        .to_string()
    }

    /// `<field>` 的 AuthOutput 响应体。
    pub fn auth_output(field: &str, access: &str, refresh: Option<&str>, has_totp: bool) -> String {
        let mut data = serde_json::Map::new();
        data.insert(
            field.to_owned(),
            json!({
                "accessToken": access,
                "refreshToken": refresh,
                "userInfo": {
                    "id": "00000000-0000-4000-8000-00000000e2e1",
                    "email": "stub@hostbee.test",
                    "hasTotp": has_totp,
                    "hasPasskey": false,
                    "emailVerified": true,
                    "phoneVerified": false,
                    "allowTicket": true
                }
            }),
        );
        json!({ "data": data }).to_string()
    }
}

impl AuthStubConfig {
    /// stub 初始 refreshToken 常量：测试预置配置文件时使用。
    pub fn initial_refresh() -> &'static str {
        "refresh-initial"
    }
}

/// 签发新的 access/refresh 对并轮换 refresh。
fn issue_pair(state: &mut AuthState) -> (String, String) {
    let n = state.access_tokens.len() + 1;
    let access = format!("access-{n}");
    let refresh = format!("refresh-{n}");
    state.access_tokens.push(access.clone());
    state.refresh_token = refresh.clone();
    (access, refresh)
}

/// 按 mode 生成响应（认证模式模拟真实后端的 token 生命周期）。
fn respond(mode: &Mode, body: &str, hb_auth: Option<&str>) -> (u16, String) {
    match mode {
        Mode::Canned { status, body } => (*status, body.clone()),
        Mode::Echo => {
            let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
            let resp = json!({
                "data": {
                    "query": parsed.get("query").cloned().unwrap_or(Value::Null),
                    "variables": parsed
                        .get("variables")
                        .cloned()
                        .unwrap_or(Value::Null),
                }
            });
            (200, serde_json::to_string(&resp).unwrap())
        }
        Mode::Auth {
            config,
            state,
            outage,
        } => {
            let mut state = state.lock().unwrap();
            let parsed: Value = serde_json::from_str(body).unwrap_or(Value::Null);
            let query = parsed.get("query").and_then(Value::as_str).unwrap_or("");
            let variables = parsed.get("variables");
            // 故障开关：认证 mutation 一律 500（真实后端瞬态故障/重启的等价模拟）。
            // 受保护查询不受影响——已签发的 access token 独立有效。
            let auth_mutation = query.contains("login(contact")
                || query.contains("verifyTotp(")
                || query.contains("refresh(refreshToken");
            if outage.load(Ordering::SeqCst) && auth_mutation {
                (500, shape::internal_error())
            } else if query.contains("login(contact") {
                let contact = variables
                    .and_then(|v| v.get("contact"))
                    .and_then(Value::as_str);
                let password = variables
                    .and_then(|v| v.get("password"))
                    .and_then(Value::as_str);
                if contact == Some(config.contact.as_str())
                    && password == Some(config.password.as_str())
                {
                    state.login_count += 1;
                    let token = format!("login-token-{}", state.login_count);
                    state.login_token = Some(token.clone());
                    (
                        200,
                        shape::auth_output("login", &token, None, config.has_totp),
                    )
                } else {
                    // 真实后端：错误密码被 normalize 成 500 internal_error
                    (500, shape::internal_error())
                }
            } else if query.contains("verifyTotp(") {
                // HB-AUTH 必须是当前 login token；验证码内容不校验——
                // 生成正确性由 unit 测试的 RFC 6238 向量覆盖，这里覆盖交换链路
                if state.login_token.is_some() && hb_auth == state.login_token.as_deref() {
                    state.totp_count += 1;
                    let (access, refresh) = issue_pair(&mut state);
                    (
                        200,
                        shape::auth_output("verifyTotp", &access, Some(&refresh), true),
                    )
                } else {
                    (400, shape::unauthorized())
                }
            } else if query.contains("refresh(refreshToken") {
                let offered = variables
                    .and_then(|v| v.get("refreshToken"))
                    .and_then(Value::as_str);
                if offered == Some(state.refresh_token.as_str()) {
                    state.refresh_count += 1;
                    let (access, refresh) = issue_pair(&mut state);
                    (
                        200,
                        shape::auth_output("refresh", &access, Some(&refresh), false),
                    )
                } else {
                    // 真实后端：旧 / 无效 refreshToken 复用 → 500 internal_error
                    (500, shape::internal_error())
                }
            } else {
                // 受保护查询：HB-AUTH 必须是 stub 签发过的 access token
                if hb_auth.is_some_and(|auth| state.access_tokens.iter().any(|t| t == auth)) {
                    (
                        200,
                        json!({
                            "data": {
                                "me": {
                                    "id": "00000000-0000-4000-8000-00000000e2e1",
                                    "email": config.contact
                                }
                            }
                        })
                        .to_string(),
                    )
                } else {
                    (400, shape::unauthorized())
                }
            }
        }
    }
}
