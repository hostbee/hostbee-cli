//! 错误模型与 stderr 输出契约。
//!
//! 输出契约（见 spec #1）：所有失败收敛为 [`CliError`]，序列化成**一行** JSON 写
//! stderr，形如 `{"errors":[...]}`；服务端返回的 GraphQL errors 数组**原样**透传，
//! 其余失败路径合成同形状的 errors 元素。stdout 在任何失败路径都不输出内容。

use serde_json::{Value, json};

/// 失败统一 exit code（成功为 0；错误原因由 stderr JSON 携带，不再细分码位）。
pub const FAILURE_EXIT_CODE: u8 = 1;

/// 所有失败路径的统一错误类型。
#[derive(Debug)]
pub enum CliError {
    /// 三种来源都没解析到 endpoint。
    NoEndpoint,
    /// `--variables` 不是合法 JSON。
    InvalidVariables(String),
    /// 配置文件存在但读取/解析失败。
    Config(String),
    /// 服务端返回的 GraphQL errors 数组非空，原样携带。
    GraphQlErrors(Vec<Value>),
    /// HTTP 传输层失败（连接拒绝、DNS 解析失败、超时等）。
    Transport(String),
    /// 服务端返回非 2xx 状态码。
    HttpStatus { status: u16, body: String },
    /// 2xx 响应但 body 不是合法的 GraphQL envelope。
    InvalidResponse(String),
}

impl CliError {
    /// 合成 errors 元素用的人类可读 message。
    fn message(&self) -> String {
        match self {
            CliError::NoEndpoint => "未提供 endpoint：请使用 --endpoint flag、\
                HOSTBEE_ENDPOINT 环境变量，或在 ~/.hostbee/config.toml 写入 endpoint"
                .to_owned(),
            CliError::InvalidVariables(err) => format!("--variables 不是合法 JSON: {err}"),
            CliError::Config(msg) => msg.clone(),
            CliError::GraphQlErrors(errors) => {
                format!("GraphQL 请求失败，服务端返回 {} 个 error", errors.len())
            }
            CliError::Transport(msg) => format!("HTTP 传输失败: {msg}"),
            CliError::HttpStatus { status, body } => {
                format!("HTTP 状态码 {status}: {}", truncate(body, 1024))
            }
            CliError::InvalidResponse(msg) => msg.clone(),
        }
    }

    /// 序列化为 stderr 一行 JSON：`{"errors":[...]}`。
    /// GraphQL errors 数组（含 extensions、locations 等任意字段）原样透传。
    pub fn stderr_json(&self) -> String {
        let errors: Vec<Value> = match self {
            CliError::GraphQlErrors(errors) => errors.clone(),
            _ => vec![json!({ "message": self.message() })],
        };
        serde_json::to_string(&json!({ "errors": errors })).expect("Value 序列化不会失败")
    }
}

/// 截断过长文本，避免错误消息吞掉整页 HTML/日志。
pub fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_owned()
    } else {
        let cut: String = s.chars().take(max_chars).collect();
        format!("{cut}…(已截断)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn graphql_errors_原样透传到_stderr_json() {
        let errors = vec![
            json!({"message": "Unknown field \"x\".", "locations": [{"line": 1, "column": 3}]}),
            json!({"message": "第二个", "extensions": {"code": "BOOM"}}),
        ];
        let err = CliError::GraphQlErrors(errors.clone());
        assert_eq!(
            serde_json::from_str::<Value>(&err.stderr_json()).unwrap(),
            json!({ "errors": errors }),
        );
    }

    #[test]
    fn 合成错误统一为_errors_形状() {
        let err = CliError::NoEndpoint;
        assert_eq!(
            serde_json::from_str::<Value>(&err.stderr_json()).unwrap(),
            json!({ "errors": [{ "message": err.message() }] }),
        );
        assert!(!err.stderr_json().contains('\n'));
    }

    #[test]
    fn http_status_携带状态码与截断后的_body() {
        let long_body = "x".repeat(3000);
        let err = CliError::HttpStatus {
            status: 502,
            body: long_body,
        };
        let parsed: Value = serde_json::from_str(&err.stderr_json()).unwrap();
        let message = parsed["errors"][0]["message"].as_str().unwrap();
        assert!(message.contains("502"));
        assert!(message.contains("已截断"));
        assert!(message.chars().count() < 1200);
    }

    #[test]
    fn transport_错误包含原因() {
        let err = CliError::Transport("Connection refused (os error 61)".to_owned());
        let parsed: Value = serde_json::from_str(&err.stderr_json()).unwrap();
        assert!(
            parsed["errors"][0]["message"]
                .as_str()
                .unwrap()
                .contains("Connection refused")
        );
    }

    #[test]
    fn truncate_不改动短文本() {
        assert_eq!(truncate("short", 1024), "short");
        assert_eq!(truncate("12345", 3), "123…(已截断)");
    }
}
