//! GraphQL 请求传输层与 envelope 分类。
//!
//! [`GraphqlTransport`] 把「POST `<endpoint>/graphql` + 可选 `HB-AUTH` 头」抽象成
//! 一个纯函数式接口：真实实现是 [`UreqTransport`]（ureq），unit 测试用脚本化实现
//! 驱动 [`crate::auth`] 的 token 生命周期状态机，不打真实网络。
//!
//! [`parse_response`] 按输出契约分类响应（纯函数，便于 unit 测试）。

use serde_json::{Value, json};

use crate::error::CliError;

/// 认证头名称：后端只认 `HB-AUTH: Bearer <accessToken>`（schema 见 webclient 一致行为）。
pub const AUTH_HEADER: &str = "HB-AUTH";

/// `<endpoint>/graphql` 拼接；endpoint 尾部多余的 `/` 去掉，避免 `//graphql`。
pub fn graphql_url(endpoint: &str) -> String {
    format!("{}/graphql", endpoint.trim_end_matches('/'))
}

/// 解析 `--variables` flag（JSON 字符串）。
pub fn parse_variables(raw: &str) -> Result<Value, CliError> {
    serde_json::from_str(raw).map_err(|e| CliError::InvalidVariables(e.to_string()))
}

/// 组装 GraphQL 请求 body：`{"query": doc, "variables": ...}`。
pub fn request_body(document: &str, variables: Option<Value>) -> Value {
    let mut body = json!({ "query": document });
    if let Some(variables) = variables {
        body["variables"] = variables;
    }
    body
}

/// 一次 GraphQL HTTP 请求的抽象。
///
/// - `endpoint`：base URL（内部拼 `/graphql`）；
/// - `auth`：非空时携带 `HB-AUTH: Bearer <auth>`；
/// - 返回 `(HTTP status, body 文本)`；传输层失败（连接拒绝等）返回 Err。
pub trait GraphqlTransport {
    fn post(
        &self,
        endpoint: &str,
        auth: Option<&str>,
        body: &Value,
    ) -> Result<(u16, String), String>;
}

/// ureq 实现（同步、阻塞式；选型见 README）。
///
/// `timeout` 为单次调用的整体超时：CLI 单次命令不设（进程短生命周期，由用户中断）；
/// daemon 等常驻进程用 [`UreqTransport::with_timeout`] 限定，避免后端挂起卡死循环。
pub struct UreqTransport {
    pub timeout: Option<std::time::Duration>,
    login_captcha: Option<String>,
}

impl UreqTransport {
    /// CLI 默认：不设超时。
    pub const fn new() -> Self {
        Self {
            timeout: None,
            login_captcha: None,
        }
    }

    /// 已验证的 CAPTCHA 凭证只用于手写登录操作，不传给后续二次验证。
    pub fn with_login_captcha(id: String) -> Self {
        Self {
            timeout: None,
            login_captcha: Some(id),
        }
    }

    /// 常驻进程用：单次调用整体超时（覆盖连接、响应与 body 读取全程）。
    pub const fn with_timeout(timeout: std::time::Duration) -> Self {
        Self {
            timeout: Some(timeout),
            login_captcha: None,
        }
    }
}

impl Default for UreqTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl GraphqlTransport for UreqTransport {
    fn post(
        &self,
        endpoint: &str,
        auth: Option<&str>,
        body: &Value,
    ) -> Result<(u16, String), String> {
        let url = graphql_url(endpoint);
        // 关闭 ureq「非 2xx 即 Err」的默认行为，拿到完整响应后按输出契约自行分类，
        // 这样非 2xx 时仍能读取 body 组装 errors。
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(self.timeout)
            .build()
            .into();
        let mut request = agent.post(&url);
        if body["query"]
            .as_str()
            .is_some_and(|q| q.starts_with("mutation HostbeeLogin("))
            && let Some(id) = &self.login_captcha
        {
            request = request.header("X-CAPTCHA-ID", id);
        }
        if let Some(token) = auth {
            request = request.header(AUTH_HEADER, format!("Bearer {token}"));
        }
        let mut response = request.send_json(body).map_err(|e| e.to_string())?;
        let status = response.status().as_u16();
        response
            .body_mut()
            .read_to_string()
            .map(|text| (status, text))
            .map_err(|e| format!("读取响应 body 失败: {e}"))
    }
}

/// 按输出契约分类响应（纯函数，便于 unit 测试）：
/// - envelope 带**非空** errors 数组 → 失败，errors 原样透传
///   （GraphQL over HTTP 允许服务端对校验/执行错误返回非 2xx，如 400，body 仍是带 errors 的 envelope）；
/// - 2xx 且 errors 缺失/为空 → Ok(data 值)；
/// - 其余（非 2xx 无 errors、2xx 但 body 不是合法 envelope）→ 收敛为对应的 [`CliError`]。
pub fn parse_response(status: u16, text: &str) -> Result<Value, CliError> {
    let success = (200..300).contains(&status);
    if let Ok(Value::Object(envelope)) = serde_json::from_str::<Value>(text) {
        if let Some(errors) = envelope.get("errors").and_then(Value::as_array)
            && !errors.is_empty()
        {
            return Err(CliError::GraphQlErrors(errors.clone()));
        }
        if success {
            return Ok(envelope.get("data").cloned().unwrap_or(Value::Null));
        }
    }
    if !success {
        return Err(CliError::HttpStatus {
            status,
            body: text.to_owned(),
        });
    }
    Err(CliError::InvalidResponse(format!(
        "响应不是合法 GraphQL envelope: {}",
        crate::error::truncate(text, 256)
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn graphql_url_去掉尾部斜杠() {
        assert_eq!(
            graphql_url("http://127.0.0.1:8000"),
            "http://127.0.0.1:8000/graphql"
        );
        assert_eq!(
            graphql_url("http://127.0.0.1:8000/"),
            "http://127.0.0.1:8000/graphql"
        );
        assert_eq!(
            graphql_url("http://127.0.0.1:8000///"),
            "http://127.0.0.1:8000/graphql"
        );
    }

    #[test]
    fn 解析变量_合法与非法_json() {
        assert_eq!(parse_variables(r#"{"a":1}"#).unwrap(), json!({"a": 1}));
        assert!(matches!(
            parse_variables("{bad json"),
            Err(CliError::InvalidVariables(_))
        ));
    }

    #[test]
    fn 请求体_无变量时不带_variables_键() {
        assert_eq!(request_body("{ x }", None), json!({ "query": "{ x }" }));
        assert_eq!(
            request_body("{ x }", Some(json!({"a": 1}))),
            json!({ "query": "{ x }", "variables": {"a": 1} })
        );
    }

    #[test]
    fn 成功响应返回_data_值() {
        let text = r#"{"data":{"backendVersion":{"version":"0.12.3"}}}"#;
        assert_eq!(
            parse_response(200, text).unwrap(),
            json!({"backendVersion": {"version": "0.12.3"}})
        );
    }

    #[test]
    fn data_为_null_时原样输出() {
        assert_eq!(
            parse_response(200, r#"{"data":null}"#).unwrap(),
            Value::Null
        );
    }

    #[test]
    fn errors_非空时失败且_原样携带() {
        let text = r#"{"data":null,"errors":[{"message":"Unknown field \"x\" on type \"QueryRoot\".","locations":[{"line":1,"column":3}]}]}"#;
        let err = parse_response(200, text).unwrap_err();
        let CliError::GraphQlErrors(errors) = err else {
            panic!("应为 GraphQlErrors: {err:?}");
        };
        assert_eq!(
            errors,
            vec![
                json!({"message": "Unknown field \"x\" on type \"QueryRoot\".",
                        "locations": [{"line": 1, "column": 3}]})
            ]
        );
    }

    #[test]
    fn errors_为空数组时视为成功() {
        assert_eq!(
            parse_response(200, r#"{"data":{"a":1},"errors":[]}"#).unwrap(),
            json!({"a": 1})
        );
    }

    #[test]
    fn 非_2xx_但_body_带_errors_时原样透传() {
        // 本地后端的真实行为：校验错误返回 400 + 带 errors 的 envelope。
        let text = r#"{"data":null,"errors":[{"message":"Unknown field \"nonexistentFieldXYZ\" on type \"QueryRoot\".","locations":[{"line":1,"column":3}]}]}"#;
        let err = parse_response(400, text).unwrap_err();
        let CliError::GraphQlErrors(errors) = err else {
            panic!("应为 GraphQlErrors: {err:?}");
        };
        assert_eq!(
            errors[0]["message"].as_str().unwrap(),
            "Unknown field \"nonexistentFieldXYZ\" on type \"QueryRoot\"."
        );
    }

    #[test]
    fn 认证失败的_400_与_真实_401_都可被识别() {
        // 本地后端真实形状：受保护字段未授权 → HTTP 400 + extensions.status 401
        let text = r#"{"data":null,"errors":[{"message":"未登录或登录已失效。","extensions":{"code":"auth.unauthorized","status":401}}]}"#;
        assert!(matches!(
            parse_response(400, text),
            Err(CliError::GraphQlErrors(_))
        ));
        // 代理/网关也可能直接给 HTTP 401
        assert!(matches!(
            parse_response(401, r#"{"error":"unauthorized"}"#),
            Err(CliError::HttpStatus { status: 401, .. })
        ));
    }

    #[test]
    fn 非_2xx_无_errors_数组时按状态码报错() {
        let err = parse_response(502, "Bad Gateway").unwrap_err();
        assert!(matches!(err, CliError::HttpStatus { status: 502, .. }));
    }

    #[test]
    fn 响应不是_json_时失败() {
        assert!(matches!(
            parse_response(200, "<html>oops</html>"),
            Err(CliError::InvalidResponse(_))
        ));
    }

    #[test]
    fn envelope_不是_object_时失败() {
        assert!(matches!(
            parse_response(200, "[1,2,3]"),
            Err(CliError::InvalidResponse(_))
        ));
    }
}
