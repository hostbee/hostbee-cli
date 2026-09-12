//! e2e harness：全仓库唯一的「高 seam」（进程边界）测试。
//!
//! 测试内启动内存 stub GraphQL server（`stub.rs`），spawn **编译好的真实二进制**
//! （`env!("CARGO_BIN_EXE_hostbee")`）指向它，断言 agent 可感知的三个通道：
//! stdout JSON、exit code、stderr 一行错误 JSON。不依赖任何真实后端（hermetic）。

mod stub;

use std::net::TcpListener;
use std::process::Command;

use serde_json::Value;
use stub::GraphqlStub;
use tempfile::TempDir;

struct RunResult {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

impl RunResult {
    /// stderr 契约：恰好一行、合法 JSON。解析前先断言形状。
    fn stderr_json(&self) -> Value {
        assert_eq!(
            self.stderr.lines().count(),
            1,
            "stderr 应恰好一行 JSON，实际为: {:?}",
            self.stderr
        );
        serde_json::from_str(self.stderr.trim_end()).expect("stderr 应为合法 JSON")
    }
}

/// spawn 编译好的 hostbee 二进制。
///
/// `env_clear` + 默认 HOME 指向空的临时目录：与真实 `~/.hostbee/` 完全隔离，
/// HOSTBEE_ENDPOINT 等环境变量也不会从测试进程泄漏；需要时通过 `envs` 显式注入
/// （注入的 HOME 覆盖默认临时目录，用于配置文件相关用例）。
fn run_hostbee(args: &[&str], envs: &[(&str, &str)]) -> RunResult {
    let home = TempDir::new().expect("创建临时 HOME 失败");
    let output = Command::new(env!("CARGO_BIN_EXE_hostbee"))
        .args(args)
        .env_clear()
        .env("HOME", home.path())
        .envs(envs.iter().copied())
        .output()
        .expect("spawn hostbee 二进制失败");
    RunResult {
        code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

/// 拿一个「保证连接被拒绝」的 endpoint：绑定后立刻释放端口。
fn refused_endpoint() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}")
}

/// 在指定 home 下写入 `~/.hostbee/config.toml`，返回该 home 的路径字符串。
fn write_config(home: &TempDir, content: &str) -> String {
    let config_dir = home.path().join(".hostbee");
    std::fs::create_dir_all(&config_dir).unwrap();
    std::fs::write(config_dir.join("config.toml"), content).unwrap();
    home.path().to_str().unwrap().to_owned()
}

#[test]
fn 成功查询_stdout_为纯_json_exit_0() {
    let stub = GraphqlStub::canned(200, r#"{"data":{"backendVersion":{"version":"0.12.3"}}}"#);
    let r = run_hostbee(
        &[
            "gql",
            "{ backendVersion { version } }",
            "--endpoint",
            stub.url(),
        ],
        &[],
    );
    assert_eq!(r.code, Some(0));
    // stdout 契约：仅一行 compact 的 data JSON，无任何其他输出
    assert_eq!(r.stdout, "{\"backendVersion\":{\"version\":\"0.12.3\"}}\n");
    assert_eq!(r.stderr, "");

    // 请求契约：document 原样透传（无改写），Content-Type 为 JSON
    let requests = stub.requests();
    assert_eq!(requests.len(), 1);
    let body: Value = serde_json::from_str(&requests[0].body).unwrap();
    assert_eq!(body["query"], "{ backendVersion { version } }");
    assert_eq!(body.get("variables"), None);
    // ureq send_json 设置 Content-Type: application/json; charset=utf-8
    assert!(
        requests[0]
            .content_type
            .as_deref()
            .is_some_and(|ct| ct.starts_with("application/json"))
    );
}

#[test]
fn graphql_errors_进_stderr_exit_非0() {
    // 与本地后端一致：校验错误走 400 + 带 errors 的 envelope
    let stub = GraphqlStub::canned(
        400,
        r#"{"data":null,"errors":[{"message":"Unknown field \"x\".","locations":[{"line":1,"column":3}],"extensions":{"code":"GRAPHQL_VALIDATION_FAILED"}}]}"#,
    );
    let r = run_hostbee(&["gql", "{ x }", "--endpoint", stub.url()], &[]);
    assert_eq!(r.code, Some(1));
    assert_eq!(r.stdout, "");
    // errors 数组原样透传（message/locations/extensions 逐字段相等）
    let expected: Value = serde_json::from_str(
        r#"{"errors":[{"message":"Unknown field \"x\".","locations":[{"line":1,"column":3}],"extensions":{"code":"GRAPHQL_VALIDATION_FAILED"}}]}"#,
    )
    .unwrap();
    assert_eq!(r.stderr_json(), expected);
}

#[test]
fn 传输失败_合成_errors_exit_非0() {
    let r = run_hostbee(&["gql", "{ x }", "--endpoint", &refused_endpoint()], &[]);
    assert_eq!(r.code, Some(1));
    assert_eq!(r.stdout, "");
    let json = r.stderr_json();
    let errors = json["errors"].as_array().expect("应有 errors 数组");
    assert_eq!(errors.len(), 1);
    assert!(errors[0]["message"].as_str().unwrap().contains("传输失败"));
}

#[test]
fn endpoint_由环境变量提供_配置不存在() {
    let stub = GraphqlStub::canned(200, r#"{"data":{"ok":true}}"#);
    // 不传 --endpoint、HOME 下无配置文件，仅靠 HOSTBEE_ENDPOINT
    let r = run_hostbee(&["gql", "{ ok }"], &[("HOSTBEE_ENDPOINT", stub.url())]);
    assert_eq!(r.code, Some(0));
    assert_eq!(r.stdout, "{\"ok\":true}\n");
    assert_eq!(stub.requests().len(), 1);
}

#[test]
fn endpoint_由配置文件提供() {
    let stub = GraphqlStub::canned(200, r#"{"data":{"who":"config"}}"#);
    let home = TempDir::new().unwrap();
    let home_str = write_config(&home, &format!("endpoint = \"{}\"\n", stub.url()));
    let r = run_hostbee(&["gql", "{ who }"], &[("HOME", home_str.as_str())]);
    assert_eq!(r.code, Some(0));
    assert_eq!(r.stdout, "{\"who\":\"config\"}\n");
    assert_eq!(stub.requests().len(), 1);
}

#[test]
fn endpoint_flag_优先于环境变量与配置文件() {
    let loser = GraphqlStub::canned(200, r#"{"data":{"who":"loser"}}"#);
    let winner = GraphqlStub::canned(200, r#"{"data":{"who":"winner"}}"#);
    let home = TempDir::new().unwrap();
    let home_str = write_config(&home, &format!("endpoint = \"{}\"\n", loser.url()));
    let r = run_hostbee(
        &["gql", "{ who }", "--endpoint", winner.url()],
        &[
            ("HOSTBEE_ENDPOINT", loser.url()),
            ("HOME", home_str.as_str()),
        ],
    );
    assert_eq!(r.code, Some(0));
    assert_eq!(r.stdout, "{\"who\":\"winner\"}\n");
    assert_eq!(winner.requests().len(), 1);
    assert_eq!(loser.requests().len(), 0);
}

#[test]
fn endpoint_环境变量优先于配置文件() {
    let loser = GraphqlStub::canned(200, r#"{"data":{"who":"loser"}}"#);
    let winner = GraphqlStub::canned(200, r#"{"data":{"who":"winner"}}"#);
    let home = TempDir::new().unwrap();
    let home_str = write_config(&home, &format!("endpoint = \"{}\"\n", loser.url()));
    let r = run_hostbee(
        &["gql", "{ who }"],
        &[
            ("HOSTBEE_ENDPOINT", winner.url()),
            ("HOME", home_str.as_str()),
        ],
    );
    assert_eq!(r.code, Some(0));
    assert_eq!(r.stdout, "{\"who\":\"winner\"}\n");
    assert_eq!(winner.requests().len(), 1);
    assert_eq!(loser.requests().len(), 0);
}

#[test]
fn 无任何_endpoint_来源时报错_exit_非0() {
    let r = run_hostbee(&["gql", "{ x }"], &[]);
    assert_eq!(r.code, Some(1));
    assert_eq!(r.stdout, "");
    let json = r.stderr_json();
    assert!(
        json["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("endpoint")
    );
}

#[test]
fn variables_原样透传() {
    let stub = GraphqlStub::echo();
    let document = "query Q($id: Int!) { f(id: $id) }";
    let r = run_hostbee(
        &[
            "gql",
            document,
            "--variables",
            r#"{"id":42,"tags":["a","b"]}"#,
            "--endpoint",
            stub.url(),
        ],
        &[],
    );
    assert_eq!(r.code, Some(0));
    assert_eq!(r.stderr, "");
    // 回显模式：data 里是 stub 收到的 query/variables，验证两侧都原样
    let echoed: Value = serde_json::from_str(r.stdout.trim_end()).unwrap();
    assert_eq!(echoed["query"], document);
    assert_eq!(
        echoed["variables"],
        serde_json::json!({"id": 42, "tags": ["a", "b"]})
    );
}

#[test]
fn variables_非法_json_本地报错且不发请求() {
    let stub = GraphqlStub::canned(200, r#"{"data":{}}"#);
    let r = run_hostbee(
        &[
            "gql",
            "{ x }",
            "--variables",
            "{bad json",
            "--endpoint",
            stub.url(),
        ],
        &[],
    );
    assert_eq!(r.code, Some(1));
    assert_eq!(r.stdout, "");
    let json = r.stderr_json();
    assert!(
        json["errors"][0]["message"]
            .as_str()
            .unwrap()
            .contains("--variables")
    );
    assert_eq!(stub.requests().len(), 0);
}

#[test]
fn http_非2xx_无_errors_envelope_时合成错误() {
    let stub = GraphqlStub::canned(500, "internal explosion");
    let r = run_hostbee(&["gql", "{ x }", "--endpoint", stub.url()], &[]);
    assert_eq!(r.code, Some(1));
    assert_eq!(r.stdout, "");
    let json = r.stderr_json();
    let message = json["errors"][0]["message"].as_str().unwrap();
    assert!(message.contains("500"));
    assert!(message.contains("internal explosion"));
}
