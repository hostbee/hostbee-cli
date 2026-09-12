//! gql 逃生门：把任意 GraphQL document 原样 POST 到 `<endpoint>/graphql`，
//! 按输出契约处理 GraphQL envelope（成功 → data 值；errors 非空/非 2xx/传输失败 → [`CliError`]）。

use serde_json::{Value, json};

use crate::error::CliError;

/// `<endpoint>/graphql` 拼接；endpoint 尾部多余的 `/` 去掉，避免 `//graphql`。
pub fn graphql_url(endpoint: &str) -> String {
    format!("{}/graphql", endpoint.trim_end_matches('/'))
}

/// 解析 `--variables` flag（JSON 字符串）。
pub fn parse_variables(raw: &str) -> Result<Value, CliError> {
    serde_json::from_str(raw).map_err(|e| CliError::InvalidVariables(e.to_string()))
}

/// 执行 GraphQL 请求：document 原样透传（不做任何改写），variables 可选。
/// 成功返回 envelope 中 `data` 的值（可能为 null）。
pub fn execute(
    endpoint: &str,
    document: &str,
    variables: Option<Value>,
) -> Result<Value, CliError> {
    let url = graphql_url(endpoint);
    let mut body = json!({ "query": document });
    if let Some(variables) = variables {
        body["variables"] = variables;
    }
    // 关闭 ureq「非 2xx 即 Err」的默认行为，拿到完整响应后按输出契约自行分类，
    // 这样非 2xx 时仍能读取 body 组装 errors。
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let mut response = agent
        .post(&url)
        .send_json(&body)
        .map_err(|e| CliError::Transport(e.to_string()))?;
    let status = response.status().as_u16();
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|e| CliError::Transport(format!("读取响应 body 失败: {e}")))?;
    parse_response(status, &text)
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
    fn 非_2xx_无_errors_数组时按状态码报错() {
        let err = parse_response(401, r#"{"error":"unauthorized"}"#).unwrap_err();
        assert!(matches!(err, CliError::HttpStatus { status: 401, .. }));
    }

    #[test]
    fn 非_2xx_状态码失败() {
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
