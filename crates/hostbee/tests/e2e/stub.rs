//! 内存 stub GraphQL HTTP server（tiny_http），e2e harness 的本地依赖。
//!
//! 两种模式：
//! - [`GraphqlStub::canned`]：固定返回给定状态码 + body（模拟成功、GraphQL errors、非 2xx 等）；
//! - [`GraphqlStub::echo`]：把收到的 query / variables 原样回填到 `data`，用于断言透传。
//!
//! 同时捕获每个请求的 Content-Type 与 body，供测试断言请求形状。

use std::sync::{Arc, Mutex};
use std::thread;

use serde_json::{Value, json};

/// stub 捕获到的一次请求。
#[derive(Clone, Debug)]
pub struct CapturedRequest {
    pub content_type: Option<String>,
    pub body: String,
}

enum Mode {
    /// 固定响应。
    Canned { status: u16, body: String },
    /// 回显 query / variables。
    Echo,
}

pub struct GraphqlStub {
    url: String,
    requests: Arc<Mutex<Vec<CapturedRequest>>>,
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

    fn start(mode: Mode) -> Self {
        let server = tiny_http::Server::http("127.0.0.1:0").expect("stub server 绑定失败");
        let url = format!("http://{}", server.server_addr());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let shared = requests.clone();
        thread::spawn(move || {
            for mut request in server.incoming_requests() {
                // 先捕获请求再响应：客户端拿到响应时，捕获必然已可见。
                let content_type = request
                    .headers()
                    .iter()
                    .find(|h| h.field.equiv("Content-Type"))
                    .map(|h| h.value.as_str().to_owned());
                let mut body = String::new();
                let _ = request.as_reader().read_to_string(&mut body);
                shared.lock().unwrap().push(CapturedRequest {
                    content_type,
                    body: body.clone(),
                });
                let (status, response_body) = match &mode {
                    Mode::Canned { status, body } => (*status, body.clone()),
                    Mode::Echo => {
                        let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
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
                };
                let _ = request.respond(
                    tiny_http::Response::from_string(response_body).with_status_code(status),
                );
            }
        });
        GraphqlStub { url, requests }
    }

    /// stub 的 base URL（传给 hostbee 的 endpoint）。
    pub fn url(&self) -> &str {
        &self.url
    }

    /// 已捕获的请求（按到达顺序）。
    pub fn requests(&self) -> Vec<CapturedRequest> {
        self.requests.lock().unwrap().clone()
    }
}
