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
    run_hostbee_input(args, envs, "")
}

fn run_hostbee_input(args: &[&str], envs: &[(&str, &str)], input: &str) -> RunResult {
    use std::io::Write;
    use std::process::Stdio;
    let home = TempDir::new().expect("创建临时 HOME 失败");
    let mut child = Command::new(env!("CARGO_BIN_EXE_hostbee"))
        .args(args)
        .env_clear()
        .env("HOME", home.path())
        .envs(envs.iter().copied())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn hostbee 二进制失败");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
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

// ========== 认证闭环（login 命令 + token 生命周期恢复） ==========

use hostbee::config::{self, Config};
use std::path::Path;
use stub::{AuthStubConfig, CapturedRequest};

const STUB_CONTACT: &str = "stub@hostbee.test";
const STUB_PASSWORD: &str = "StubPass!123";
/// RFC 6238 SHA-256 测试种子的 Base32（stub 不校验验证码内容；算法由 RFC 向量 unit test 验证）
const TOTP_BASE32: &str = "GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQGEZA";

fn auth_stub(has_totp: bool) -> GraphqlStub {
    GraphqlStub::auth(AuthStubConfig {
        contact: STUB_CONTACT.to_owned(),
        password: STUB_PASSWORD.to_owned(),
        has_totp,
    })
}

/// 在独立临时 home 下执行 `hostbee login`（TOTP 账号），断言成功并返回 home 与 stdout JSON。
fn do_login(stub: &GraphqlStub) -> (TempDir, String, serde_json::Value) {
    let home = TempDir::new().unwrap();
    let home_str = home.path().to_str().unwrap().to_owned();
    let r = run_hostbee(
        &[
            "login",
            "--contact",
            STUB_CONTACT,
            "--password",
            STUB_PASSWORD,
            "--totp-secret",
            TOTP_BASE32,
            "--endpoint",
            stub.url(),
        ],
        &[("HOME", home_str.as_str())],
    );
    assert_eq!(r.code, Some(0), "login 应成功: stderr={}", r.stderr);
    let stdout: serde_json::Value =
        serde_json::from_str(r.stdout.trim_end()).expect("stdout 应为 AuthOutput JSON");
    (home, home_str, stdout)
}

/// 读取临时 home 下落盘的配置。
fn read_config(home: &str) -> Config {
    let path = Path::new(home).join(".hostbee").join("config.toml");
    config::read_config(&path)
        .unwrap()
        .expect("登录后配置应已落盘")
}

/// 修改临时 home 下的配置（模拟 token 损坏 / 失效等场景）。
fn overwrite_config(home: &str, mutate: impl FnOnce(&mut Config)) {
    let path = Path::new(home).join(".hostbee").join("config.toml");
    let mut cfg = config::read_config(&path).unwrap().expect("配置应存在");
    mutate(&mut cfg);
    config::write_config_atomic(&path, &cfg).unwrap();
}

/// 到达 stub 的请求中包含指定 mutation 的**尝试次数**（无论成败）。
fn attempt_count(stub: &GraphqlStub, needle: &str) -> usize {
    stub.requests()
        .iter()
        .filter(|req| req.body.contains(needle))
        .count()
}

#[test]
fn login_base32_支持参数环境变量和配置文件() {
    for source in ["flag", "env", "config"] {
        let stub = auth_stub(true);
        let home = TempDir::new().unwrap();
        let home_str = home.path().to_str().unwrap();
        let secret = format!(" \t{}====\n", TOTP_BASE32.to_ascii_lowercase());
        let path = home.path().join(".hostbee/config.toml");
        if source == "config" {
            config::write_config_atomic(
                &path,
                &Config {
                    totp_secret: Some(secret.clone()),
                    ..Default::default()
                },
            )
            .unwrap();
        }
        let mut args = vec![
            "login",
            "--endpoint",
            stub.url(),
            "--contact",
            STUB_CONTACT,
            "--password",
            STUB_PASSWORD,
        ];
        let mut env = vec![("HOME", home_str)];
        if source == "flag" {
            args.extend(["--totp-secret", secret.as_str()]);
        } else if source == "env" {
            env.push(("HOSTBEE_TOTP_SECRET", secret.as_str()));
        }
        let r = run_hostbee(&args, &env);
        assert_eq!(r.code, Some(0), "{source}: {}", r.stderr);
        let output: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
        assert_eq!(output["refreshToken"], "refresh-1");
        assert_eq!(
            read_config(home_str).totp_secret.as_deref(),
            Some(secret.as_str())
        );
        assert_eq!(attempt_count(&stub, "sendVerificationCode("), 0);
        let request = stub
            .requests()
            .into_iter()
            .find(|r| r.body.contains("verifyTotp("))
            .unwrap();
        let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
        let code = body["variables"]["code"].as_str().unwrap();
        assert_eq!(code.len(), 6);
        assert!(code.bytes().all(|b| b.is_ascii_digit()));
    }
}

#[test]
fn login_非法_base32_不发送验证码或覆盖配置() {
    for secret in ["31323334353637383930", "MZXW6=", "密钥"] {
        let stub = auth_stub(true);
        let (home, home_str, _) = do_login(&stub);
        let path = home.path().join(".hostbee/config.toml");
        let before = std::fs::read(&path).unwrap();
        let r = run_hostbee(
            &[
                "login",
                "--contact",
                STUB_CONTACT,
                "--password",
                STUB_PASSWORD,
                "--totp-secret",
                secret,
            ],
            &[("HOME", &home_str)],
        );
        assert_eq!(r.code, Some(1));
        assert!(r.stdout.is_empty());
        let error = r.stderr_json();
        assert!(
            error["errors"][0]["message"]
                .as_str()
                .unwrap()
                .contains("Base32")
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(attempt_count(&stub, "verifyTotp("), 1); // 仅 setup 登录
        assert_eq!(attempt_count(&stub, "sendVerificationCode("), 0);
    }
}

#[test]
fn login_totp_账号_自动完成交换_凭据明文落盘() {
    let stub = auth_stub(true);
    let (home, home_str, stdout) = do_login(&stub);

    // stdout：AuthOutput 同构 JSON——token 对来自 verifyTotp 交换
    assert_eq!(stdout["accessToken"], "access-1");
    assert_eq!(stdout["refreshToken"], "refresh-1");
    assert_eq!(stdout["userInfo"]["email"], "stub@hostbee.test");
    assert_eq!(stdout["userInfo"]["hasTotp"], true);

    // 凭据明文落盘（ADR-0001）：endpoint + contact + password + totp_secret + token 对
    let cfg = read_config(&home_str);
    assert_eq!(cfg.endpoint.as_deref(), Some(stub.url()));
    assert_eq!(cfg.contact.as_deref(), Some(STUB_CONTACT));
    assert_eq!(cfg.password.as_deref(), Some(STUB_PASSWORD));
    assert_eq!(cfg.totp_secret.as_deref(), Some(TOTP_BASE32));
    assert_eq!(cfg.access_token.as_deref(), Some("access-1"));
    assert_eq!(cfg.refresh_token.as_deref(), Some("refresh-1"));

    // stub 侧：login + verifyTotp 交换各一次；login 请求不带认证头
    let state = stub.auth_state().unwrap();
    assert_eq!(
        (state.login_count, state.totp_count, state.refresh_count),
        (1, 1, 0)
    );
    let requests = stub.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].hb_auth, None);
    assert_eq!(requests[1].hb_auth.as_deref(), Some("login-token-1"));
    let _ = home;
}

#[test]
fn login_邮件验证_启用或未启用_totp_均取得完整凭据() {
    for has_totp in [true, false] {
        let stub = auth_stub(has_totp);
        let home = TempDir::new().unwrap();
        let home_str = home.path().to_str().unwrap().to_owned();
        let r = run_hostbee_input(
            &[
                "login",
                "--contact",
                STUB_CONTACT,
                "--password",
                STUB_PASSWORD,
                "--endpoint",
                stub.url(),
            ],
            &[("HOME", home_str.as_str())],
            "123456\n",
        );
        assert_eq!(r.code, Some(0), "{}", r.stderr);
        let stdout: serde_json::Value = serde_json::from_str(r.stdout.trim_end()).unwrap();
        assert_eq!(stdout["accessToken"], "access-1");
        assert_eq!(stdout["refreshToken"], "refresh-1");
        assert!(r.stderr.contains("邮件验证码"), "{}", r.stderr);
        assert_eq!(r.stdout.lines().count(), 1);
        let cfg = read_config(&home_str);
        assert_eq!(cfg.access_token.as_deref(), Some("access-1"));
        assert_eq!(cfg.refresh_token.as_deref(), Some("refresh-1"));
        assert_eq!(cfg.totp_secret, None);
        assert_eq!(cfg.contact.as_deref(), Some(STUB_CONTACT));
        assert_eq!(cfg.password.as_deref(), Some(STUB_PASSWORD));
        assert_eq!(cfg.endpoint.as_deref(), Some(stub.url()));
        // stub：无 TOTP 交换
        let state = stub.auth_state().unwrap();
        assert_eq!((state.login_count, state.totp_count), (1, 0));
        let requests = stub.requests();
        assert_eq!(requests.len(), 3);
        for request in &requests[1..] {
            assert_eq!(request.hb_auth.as_deref(), Some("login-token-1"));
        }
        let query = run_hostbee(&["gql", "{ me { id } }"], &[("HOME", &home_str)]);
        assert_eq!(query.code, Some(0), "{}", query.stderr);
    }
}

#[test]
fn login_邮件验证码输入结束_报错且不落盘() {
    let stub = auth_stub(true);
    let home = TempDir::new().unwrap();
    let home_str = home.path().to_str().unwrap().to_owned();
    let r = run_hostbee(
        &[
            "login",
            "--contact",
            STUB_CONTACT,
            "--password",
            STUB_PASSWORD,
            "--endpoint",
            stub.url(),
        ],
        &[("HOME", home_str.as_str())],
    );
    assert_eq!(r.code, Some(1));
    assert_eq!(r.stdout, "");
    assert!(r.stderr.contains("未输入 邮件验证码"), "{}", r.stderr);
    assert!(stub.auth_state().unwrap().email_sent);
    assert_eq!(attempt_count(&stub, "verifyVerificationCode("), 0);
    assert!(
        !Path::new(&home_str)
            .join(".hostbee")
            .join("config.toml")
            .exists()
    );
    // login 已发出、交换未发出
    let state = stub.auth_state().unwrap();
    assert_eq!((state.login_count, state.totp_count), (1, 0));
}

#[test]
fn 登录后_gql_自动携带_hb_auth() {
    let stub = auth_stub(true);
    let (_home, home_str, _) = do_login(&stub);
    // 不带 --endpoint：endpoint 来自落盘的配置文件
    let r = run_hostbee(&["gql", "{ me { id } }"], &[("HOME", home_str.as_str())]);
    assert_eq!(r.code, Some(0), "stderr: {}", r.stderr);
    // 结构比较（不依赖 JSON 键序）
    let stdout: serde_json::Value = serde_json::from_str(r.stdout.trim_end()).unwrap();
    assert_eq!(
        stdout,
        serde_json::json!({"me": {"id": "00000000-0000-4000-8000-00000000e2e1", "email": "stub@hostbee.test"}})
    );
    // 第三个请求是 gql，HB-AUTH 为交换拿到的 access token（login 请求不带认证头）
    let requests = stub.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].hb_auth, None);
    assert_eq!(requests[1].hb_auth.as_deref(), Some("login-token-1"));
    assert_eq!(requests[2].hb_auth.as_deref(), Some("access-1"));
}

#[test]
fn login_邮件验证码错误或空输入_不覆盖已有配置() {
    for input in ["000000\n", "\n", ""] {
        let stub = auth_stub(true);
        let home = TempDir::new().unwrap();
        let original = format!(
            "endpoint = \"{}\"\naccess_token = \"old-access\"\nrefresh_token = \"old-refresh\"\n",
            stub.url()
        );
        let home_str = write_config(&home, &original);
        let r = run_hostbee_input(
            &[
                "login",
                "--contact",
                STUB_CONTACT,
                "--password",
                STUB_PASSWORD,
            ],
            &[("HOME", &home_str)],
            input,
        );
        assert_eq!(r.code, Some(1), "{}", r.stderr);
        assert!(r.stdout.is_empty());
        assert!(r.stderr.contains(if input == "000000\n" {
            "Invalid verification code"
        } else {
            "未输入 邮件验证码"
        }));
        assert_eq!(
            std::fs::read_to_string(home.path().join(".hostbee/config.toml")).unwrap(),
            original
        );
        assert_eq!(attempt_count(&stub, "sendVerificationCode"), 1);
        assert_eq!(
            attempt_count(&stub, "verifyVerificationCode("),
            usize::from(input == "000000\n")
        );
    }
}

#[test]
fn login_邮件发送故障和过期及不完整响应_均保留已有配置() {
    let login = serde_json::json!({"data": {"login": {"accessToken": "login-only", "refreshToken": null, "userInfo": {"hasTotp": true}}}}).to_string();
    let sent = serde_json::json!({"data": {"sendVerificationCode": true}}).to_string();
    for (responses, expected, requests) in [
        (vec![(503, r#"{"errors":[{"message":"Mail unavailable"}]}"#.to_owned())], "Mail unavailable", 2),
        (vec![(200, r#"{"data":{"sendVerificationCode":false}}"#.to_owned())], "未发送成功", 2),
        (vec![(200, sent.clone()), (200, r#"{"errors":[{"message":"Verification code has expired"}]}"#.to_owned())], "Verification code has expired", 3),
        (vec![(200, sent), (200, r#"{"data":{"verifyVerificationCode":{"accessToken":"partial","refreshToken":null}}}"#.to_owned())], "缺少 refreshToken", 3),
    ] {
        let mut replies = vec![(200, login.clone())];
        replies.extend(responses);
        let stub = GraphqlStub::scripted(replies);
        let home = TempDir::new().unwrap();
        let original = format!("endpoint = \"{}\"\naccess_token = \"old\"\n", stub.url());
        let home_str = write_config(&home, &original);
        let r = run_hostbee_input(
            &["login", "--contact", STUB_CONTACT, "--password", STUB_PASSWORD],
            &[("HOME", &home_str)], "123456\n",
        );
        assert_eq!(r.code, Some(1));
        assert!(r.stdout.is_empty());
        assert!(r.stderr.contains(expected), "{}", r.stderr);
        assert_eq!(stub.requests().len(), requests);
        assert_eq!(std::fs::read_to_string(home.path().join(".hostbee/config.toml")).unwrap(), original);
    }
}

#[test]
fn 邮件登录后_refresh可恢复_失效后查询不发邮件不覆盖凭据() {
    let stub = auth_stub(false);
    let home = TempDir::new().unwrap();
    let home_str = home.path().to_str().unwrap();
    let login = run_hostbee_input(
        &[
            "login",
            "--contact",
            STUB_CONTACT,
            "--password",
            STUB_PASSWORD,
            "--endpoint",
            stub.url(),
        ],
        &[("HOME", home_str)],
        "123456\n",
    );
    assert_eq!(login.code, Some(0), "{}", login.stderr);
    overwrite_config(home_str, |cfg| {
        cfg.access_token = Some("expired".to_owned())
    });
    let query = run_hostbee(&["gql", "{ me { id } }"], &[("HOME", home_str)]);
    assert_eq!(query.code, Some(0), "{}", query.stderr);
    assert_eq!(
        read_config(home_str).refresh_token.as_deref(),
        Some("refresh-2")
    );
    overwrite_config(home_str, |cfg| {
        cfg.access_token = Some("expired".to_owned());
        cfg.refresh_token = Some("revoked".to_owned());
    });
    let path = home.path().join(".hostbee/config.toml");
    let original = std::fs::read_to_string(&path).unwrap();
    let query = run_hostbee(&["gql", "{ me { id } }"], &[("HOME", home_str)]);
    assert_eq!(query.code, Some(1));
    assert!(query.stdout.is_empty());
    assert!(query.stderr_json().to_string().contains("hostbee login"));
    assert_eq!(attempt_count(&stub, "sendVerificationCode"), 1);
    assert_eq!(std::fs::read_to_string(path).unwrap(), original);
}

#[test]
fn access_token_失效_自动_refresh_轮换_重试成功_落盘() {
    let stub = auth_stub(true);
    let (_home, home_str, _) = do_login(&stub);
    // 模拟 access token 过期/损坏
    overwrite_config(&home_str, |cfg| {
        cfg.access_token = Some("corrupted-token".to_owned());
    });
    let r = run_hostbee(&["gql", "{ me { id } }"], &[("HOME", home_str.as_str())]);
    assert_eq!(r.code, Some(0), "stderr: {}", r.stderr);
    assert!(r.stdout.contains("\"me\""));

    // stub 侧：恰好一次 refresh 轮换，不触发密码重登
    let state = stub.auth_state().unwrap();
    assert_eq!(state.refresh_count, 1);
    assert_eq!(state.login_count, 1, "不应触发密码重登");
    // 磁盘上的配置文件已原子更新为轮换后的新对
    let cfg = read_config(&home_str);
    assert_eq!(cfg.access_token.as_deref(), Some("access-2"));
    assert_eq!(cfg.refresh_token.as_deref(), Some("refresh-2"));
    // 重试请求确实带了新 access token
    let requests = stub.requests();
    let gql_requests: Vec<&CapturedRequest> = requests
        .iter()
        .filter(|req| req.body.contains("{ me { id } }"))
        .collect();
    assert_eq!(gql_requests.len(), 2);
    assert_eq!(gql_requests[0].hb_auth.as_deref(), Some("corrupted-token"));
    assert_eq!(gql_requests[1].hb_auth.as_deref(), Some("access-2"));
}

#[test]
fn refresh_token_失效_密码自动重登_重试成功() {
    let stub = auth_stub(true);
    let (_home, home_str, _) = do_login(&stub);
    overwrite_config(&home_str, |cfg| {
        cfg.access_token = Some("corrupted-token".to_owned());
        cfg.refresh_token = Some("revoked-refresh".to_owned());
    });
    let r = run_hostbee(&["gql", "{ me { id } }"], &[("HOME", home_str.as_str())]);
    assert_eq!(r.code, Some(0), "stderr: {}", r.stderr);

    // refresh 试过一次但失败（成功计数为 0），密码重登 + TOTP 交换成功
    let state = stub.auth_state().unwrap();
    assert_eq!(attempt_count(&stub, "refresh(refreshToken"), 1);
    assert_eq!(state.refresh_count, 0);
    assert_eq!(state.login_count, 2);
    assert_eq!(state.totp_count, 2);
    // 磁盘配置已更新为重登拿到的新对
    let cfg = read_config(&home_str);
    assert_eq!(cfg.access_token.as_deref(), Some("access-2"));
    assert_eq!(cfg.refresh_token.as_deref(), Some("refresh-2"));
}

#[test]
fn 全部失效_原错误透传_exit_非0() {
    let stub = auth_stub(true);
    let (_home, home_str, _) = do_login(&stub);
    overwrite_config(&home_str, |cfg| {
        cfg.access_token = Some("corrupted-token".to_owned());
        cfg.refresh_token = Some("revoked-refresh".to_owned());
        cfg.password = Some("WrongPassword1!".to_owned());
    });
    let r = run_hostbee(&["gql", "{ me { id } }"], &[("HOME", home_str.as_str())]);
    assert_eq!(r.code, Some(1));
    assert_eq!(r.stdout, "");
    let json = r.stderr_json();
    let errors = json["errors"].as_array().unwrap();
    // 首条为原始认证错误的原样透传（stub 模拟真实后端：400 + extensions.status 401）
    assert_eq!(errors[0]["extensions"]["code"], "auth.unauthorized");
    assert_eq!(errors[0]["extensions"]["status"], 401);
    // 恢复过程附注可程序化读取
    let messages: Vec<&str> = errors
        .iter()
        .filter_map(|e| e["message"].as_str())
        .collect();
    assert!(messages.iter().any(|m| m.contains("refresh 轮换未成功")));
    assert!(messages.iter().any(|m| m.contains("密码重新登录未成功")));
    // stub 侧：重登尝试过一次（密码错 → 500）。成功计数只含 setup 登录，
    // 尝试次数（含失败）用捕获请求统计
    let state = stub.auth_state().unwrap();
    assert_eq!(attempt_count(&stub, "login(contact"), 2);
    assert_eq!(state.login_count, 1);
}

#[test]
fn env_refresh_token_覆盖配置_失效时兜底配置候选() {
    let stub = auth_stub(true);
    let (_home, home_str, _) = do_login(&stub);
    overwrite_config(&home_str, |cfg| {
        cfg.access_token = Some("corrupted-token".to_owned());
    });
    let r = run_hostbee(
        &["gql", "{ me { id } }"],
        &[
            ("HOME", home_str.as_str()),
            ("HOSTBEE_REFRESH_TOKEN", "stale-env-refresh"),
        ],
    );
    assert_eq!(r.code, Some(0), "stderr: {}", r.stderr);
    // env 旧值先试（失败），config 中轮换后的值兜底成功——共两次 refresh 尝试。
    // 成功计数只含兜底那次；尝试次数（含失败）用捕获请求统计
    let state = stub.auth_state().unwrap();
    assert_eq!(attempt_count(&stub, "refresh(refreshToken"), 2);
    assert_eq!(state.refresh_count, 1);
    let cfg = read_config(&home_str);
    assert_eq!(cfg.refresh_token.as_deref(), Some("refresh-2"));
}

#[test]
fn env_refresh_token_无本地凭据_也能完成_并尽力落盘() {
    let stub = auth_stub(true);
    let home = TempDir::new().unwrap();
    let home_str = home.path().to_str().unwrap().to_owned();
    // 无配置文件：仅靠 HOSTBEE_ENDPOINT + HOSTBEE_REFRESH_TOKEN（stub 的初始 refresh）
    let r = run_hostbee(
        &["gql", "{ me { id } }"],
        &[
            ("HOME", home_str.as_str()),
            ("HOSTBEE_ENDPOINT", stub.url()),
            ("HOSTBEE_REFRESH_TOKEN", AuthStubConfig::initial_refresh()),
        ],
    );
    assert_eq!(r.code, Some(0), "stderr: {}", r.stderr);
    // 401 → env refresh 轮换 → 重试成功
    let state = stub.auth_state().unwrap();
    assert_eq!(state.refresh_count, 1);
    assert_eq!(state.login_count, 0);
    // 新 token 对尽力落盘：下次调用直接复用
    let cfg = read_config(&home_str);
    assert_eq!(cfg.endpoint.as_deref(), Some(stub.url()));
    assert_eq!(cfg.access_token.as_deref(), Some("access-1"));
    assert_eq!(cfg.refresh_token.as_deref(), Some("refresh-1"));
    assert_eq!(cfg.contact, None);
    assert_eq!(cfg.password, None);
}

// ========== daemon 保活（ticket #6） ==========

use std::io::{BufRead, BufReader};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 轮询等待条件成立（100ms 间隔）；超时返回最后一次求值结果。
fn wait_until(timeout: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if cond() {
            return true;
        }
        if Instant::now() >= deadline {
            return cond();
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// 运行中的 daemon 子进程：stderr 管道实时采集到共享缓冲，可随时断言日志。
struct DaemonProc {
    child: std::process::Child,
    stderr_buf: Arc<Mutex<String>>,
}

impl DaemonProc {
    /// spawn `hostbee daemon ...`（envs 必须含 HOME；stdout 丢弃——契约要求恒空）。
    fn spawn(args: &[&str], envs: &[(&str, &str)]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_hostbee"))
            .args(args)
            .env_clear()
            .envs(envs.iter().copied())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn hostbee daemon 失败");
        let pipe = child.stderr.take().expect("daemon stderr 应为管道");
        let stderr_buf = Arc::new(Mutex::new(String::new()));
        let buf = stderr_buf.clone();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(pipe);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => buf.lock().unwrap().push_str(&line),
                }
            }
        });
        DaemonProc { child, stderr_buf }
    }

    /// 目前为止采集到的 stderr 全文。
    fn stderr(&self) -> String {
        self.stderr_buf.lock().unwrap().clone()
    }

    /// SIGTERM 后等待干净退出（exit 0）；超时则 SIGKILL 并原样返回状态（断言会失败）。
    /// 返回最终退出状态与补齐后的 stderr 全文（退出日志行可能略晚于 wait 返回）。
    fn sigterm_and_wait(self, timeout: Duration) -> (std::process::ExitStatus, String) {
        let mut child = self.child;
        let buf = self.stderr_buf;
        let pid = child.id().to_string();
        let _ = Command::new("kill").arg("-TERM").arg(&pid).status();
        let deadline = Instant::now() + timeout;
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                break child.wait().unwrap();
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        // 采集线程可能还有最后一两行未入缓冲，短暂等待补齐
        let _ = wait_until(Duration::from_millis(500), || {
            buf.lock().unwrap().contains("收到终止信号，退出")
        });
        (status, buf.lock().unwrap().clone())
    }
}

/// 判定本机是否 systemd 环境（bare daemon 在 systemd 机器上会真实安装服务，
/// 该场景的 e2e 只在无 systemd 机器上运行）。
fn systemd_machine() -> bool {
    Path::new("/run/systemd/system").is_dir()
}

#[test]
fn daemon_前台保活_持续轮换_cli_全程可用_sigterm_干净退出() {
    let stub = auth_stub(true);
    let (_home, home_str, _) = do_login(&stub);
    // 首轮立即执行（t=0），此后每 1s 一轮
    let daemon = DaemonProc::spawn(
        &["daemon", "--interval", "1", "--foreground"],
        &[("HOME", home_str.as_str())],
    );
    assert!(
        wait_until(Duration::from_secs(10), || {
            stub.auth_state().unwrap().refresh_count >= 3
        }),
        "daemon 应完成至少 3 次轮换，实际 {}，stderr：{}",
        stub.auth_state().unwrap().refresh_count,
        daemon.stderr()
    );

    // 轮换期间并发跑普通 CLI 查询：全程可用（读最新落盘 token）
    for _ in 0..3 {
        let r = run_hostbee(&["gql", "{ me { id } }"], &[("HOME", home_str.as_str())]);
        assert_eq!(r.code, Some(0), "daemon 运行期间 CLI 应可用: {}", r.stderr);
        assert!(r.stdout.contains("\"me\""));
    }

    // SIGTERM：干净退出（exit 0 + 退出日志）
    let (status, stderr) = daemon.sigterm_and_wait(Duration::from_secs(5));
    assert_eq!(
        status.code(),
        Some(0),
        "SIGTERM 应干净退出，stderr：{stderr}"
    );
    assert!(
        stderr.contains("refresh 轮换成功"),
        "应记录轮换日志：{stderr}"
    );
    assert!(
        stderr.contains("收到终止信号，退出"),
        "应记录退出日志：{stderr}"
    );
    // 每行日志都带 [时间戳] 前缀
    assert!(
        stderr
            .lines()
            .all(|l| l.starts_with('[') || l.starts_with("警告")),
        "daemon stderr 应全为带时间戳的日志行：{stderr}"
    );

    // 磁盘配置已轮换，且与 stub 服务端当前有效 refreshToken 一致（原子写无半更新）
    let cfg = read_config(&home_str);
    let state = stub.auth_state().unwrap();
    assert_eq!(
        cfg.refresh_token.as_deref(),
        Some(state.refresh_token.as_str())
    );
    // daemon 只做 refresh：不触发密码重登（login 仍是 setup 那一次）
    assert_eq!(state.login_count, 1);

    // daemon 退出后 CLI 仍可用（token 对已持久化，不依赖 daemon 存活）
    let r = run_hostbee(&["gql", "{ me { id } }"], &[("HOME", home_str.as_str())]);
    assert_eq!(r.code, Some(0), "daemon 退出后 CLI 应仍可用: {}", r.stderr);
}

#[test]
fn daemon_refresh_连续失败_退避日志_故障恢复后自愈() {
    let stub = auth_stub(true);
    let (_home, home_str, _) = do_login(&stub);
    // 后端「挂掉」：认证 mutation 一律 500
    stub.set_outage(true);
    let daemon = DaemonProc::spawn(
        &["daemon", "--interval", "1", "--foreground"],
        &[("HOME", home_str.as_str())],
    );
    assert!(
        wait_until(Duration::from_secs(10), || daemon.stderr().contains("退避")),
        "失败应产生退避日志，stderr：{}",
        daemon.stderr()
    );
    // 失败期间：配置未轮换（仍是 login 落盘的 refresh-1）、无成功轮换
    let cfg = read_config(&home_str);
    assert_eq!(cfg.refresh_token.as_deref(), Some("refresh-1"));
    let state = stub.auth_state().unwrap();
    assert_eq!(state.refresh_count, 0, "故障期间不应有成功轮换");
    // 失败期间 CLI 仍可用：已落盘 access token 独立有效，daemon 故障不影响 CLI
    let r = run_hostbee(&["gql", "{ me { id } }"], &[("HOME", home_str.as_str())]);
    assert_eq!(r.code, Some(0), "故障期间 CLI 应仍可用: {}", r.stderr);

    // 故障恢复：退避节奏下自愈（interval=1 → 退避恒 1s）
    stub.set_outage(false);
    assert!(
        wait_until(Duration::from_secs(10), || {
            stub.auth_state().unwrap().refresh_count >= 1
        }),
        "故障恢复后应自愈完成轮换，stderr：{}",
        daemon.stderr()
    );
    let stderr = daemon.stderr();
    assert!(stderr.contains("保活失败"), "应有失败日志：{stderr}");
    assert!(
        stderr.contains("refresh 轮换成功"),
        "恢复后应有成功日志：{stderr}"
    );
    let cfg = read_config(&home_str);
    let state = stub.auth_state().unwrap();
    assert_eq!(
        cfg.refresh_token.as_deref(),
        Some(state.refresh_token.as_str())
    );

    let (status, stderr) = daemon.sigterm_and_wait(Duration::from_secs(5));
    assert_eq!(status.code(), Some(0), "stderr：{stderr}");
}

#[test]
fn daemon_无_foreground_非_systemd_降级前台循环() {
    if systemd_machine() {
        // bare daemon 在 systemd 机器上会真实安装并 enable 服务（预期行为），
        // e2e 不该动真实系统配置；安装路径由 unit 测试（假 systemctl）覆盖。
        return;
    }
    let stub = auth_stub(true);
    let (_home, home_str, _) = do_login(&stub);
    // 不带 --foreground：macOS/容器上应明确提示降级并直接前台循环
    let daemon = DaemonProc::spawn(
        &["daemon", "--interval", "1"],
        &[("HOME", home_str.as_str())],
    );
    assert!(
        wait_until(Duration::from_secs(10), || {
            daemon.stderr().contains("未检测到 systemd")
                && stub.auth_state().unwrap().refresh_count >= 2
        }),
        "应提示降级并前台循环轮换，stderr：{}",
        daemon.stderr()
    );
    let (status, stderr) = daemon.sigterm_and_wait(Duration::from_secs(5));
    assert_eq!(status.code(), Some(0), "stderr：{stderr}");
    assert!(
        stderr.contains("未检测到 systemd"),
        "应有降级提示：{stderr}"
    );
}

#[test]
fn daemon_旧refresh已撤销_密码重登自愈() {
    let stub = auth_stub(true);
    let (_home, home_str, _) = do_login(&stub);
    // 模拟长期离线后 refreshToken 被服务端 revoke（例如别处 CLI 已轮换过）
    overwrite_config(&home_str, |cfg| {
        cfg.refresh_token = Some("revoked-refresh".to_owned());
    });
    let daemon = DaemonProc::spawn(
        &["daemon", "--interval", "1", "--foreground"],
        &[("HOME", home_str.as_str())],
    );
    assert!(
        wait_until(Duration::from_secs(10), || {
            stub.auth_state().unwrap().login_count >= 2
        }),
        "daemon 应在 refresh 失败后密码重登自愈，stderr：{}",
        daemon.stderr()
    );
    let (status, stderr) = daemon.sigterm_and_wait(Duration::from_secs(5));
    assert_eq!(status.code(), Some(0), "stderr：{stderr}");
    assert!(
        stderr.contains("密码重登恢复会话成功"),
        "应有重登成功日志：{stderr}"
    );
    // 恰一次重登（setup login + daemon 自愈一次），TOTP 交换随之完成；
    // 自愈成功后后续周期走正常 refresh，不再触发重登
    let state = stub.auth_state().unwrap();
    assert_eq!(state.login_count, 2);
    assert_eq!(state.totp_count, 2);
    assert!(attempt_count(&stub, "refresh(refreshToken") >= 1);
    // 配置已轮换为重登拿到的新对（与 stub 当前有效 refreshToken 一致）
    let cfg = read_config(&home_str);
    assert_eq!(
        cfg.refresh_token.as_deref(),
        Some(state.refresh_token.as_str())
    );
}

#[test]
fn daemon_无任何凭据_致命退出_一行json错误() {
    let home = TempDir::new().unwrap();
    let home_str = home.path().to_str().unwrap().to_owned();
    // 空 HOME + 仅 env endpoint：无 refreshToken 也无 contact+password
    let r = run_hostbee(
        &["daemon", "--interval", "1", "--foreground"],
        &[
            ("HOME", home_str.as_str()),
            ("HOSTBEE_ENDPOINT", "http://127.0.0.1:1"),
        ],
    );
    assert_eq!(r.code, Some(1));
    assert_eq!(r.stdout, "", "daemon stdout 恒空");
    let json = r.stderr_json();
    let message = json["errors"][0]["message"].as_str().unwrap();
    assert!(message.contains("凭据"), "应提示凭据缺失: {message}");
}

#[test]
fn daemon_邮件验证账号恢复失败_退避且不发邮件不覆盖配置() {
    let stub = auth_stub(true);
    let home = TempDir::new().unwrap();
    let original = format!(
        "endpoint = \"{}\"\ncontact = \"{}\"\npassword = \"{}\"\naccess_token = \"old-access\"\nrefresh_token = \"revoked\"\n",
        stub.url(),
        STUB_CONTACT,
        STUB_PASSWORD
    );
    let home_str = write_config(&home, &original);
    let daemon = DaemonProc::spawn(
        &["daemon", "--foreground", "--interval", "1"],
        &[("HOME", &home_str)],
    );
    let failed = wait_until(Duration::from_secs(10), || {
        daemon.stderr().contains("hostbee login")
    });
    let (status, stderr) = daemon.sigterm_and_wait(Duration::from_secs(5));
    assert!(failed, "{stderr}");
    assert_eq!(status.code(), Some(0), "{stderr}");
    assert!(stderr.contains("退避"), "{stderr}");
    assert_eq!(attempt_count(&stub, "sendVerificationCode"), 0);
    assert_eq!(attempt_count(&stub, "verifyVerificationCode("), 0);
    assert_eq!(
        std::fs::read_to_string(home.path().join(".hostbee/config.toml")).unwrap(),
        original
    );
}

#[test]
fn login_全交互读取账号密码邮件验证码() {
    let stub = auth_stub(true);
    let home = TempDir::new().unwrap();
    let home_str = write_config(&home, &format!("endpoint = \"{}\"\n", stub.url()));
    let r = run_hostbee_input(
        &["login"],
        &[("HOME", &home_str)],
        &format!("{STUB_CONTACT}\n{STUB_PASSWORD}\n123456\n"),
    );
    assert_eq!(r.code, Some(0), "{}", r.stderr);
    assert!(
        r.stderr.contains("contact")
            && r.stderr.contains("password")
            && r.stderr.contains("邮件验证码")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&r.stdout).unwrap()["refreshToken"],
        "refresh-1"
    );
    assert_eq!(
        read_config(&home_str).refresh_token.as_deref(),
        Some("refresh-1")
    );
}

#[test]
fn daemon_interval_0_拒绝() {
    let r = run_hostbee(&["daemon", "--interval", "0"], &[]);
    assert_eq!(r.code, Some(1));
    assert_eq!(r.stdout, "");
    let json = r.stderr_json();
    let message = json["errors"][0]["message"].as_str().unwrap();
    assert!(
        message.contains("interval"),
        "应提示 interval 非法: {message}"
    );
}

#[test]
fn daemon_interval_不小于_7天_tll_有警告仍继续检查凭据() {
    let home = TempDir::new().unwrap();
    let home_str = home.path().to_str().unwrap().to_owned();
    // interval >= 604800：先警告（带时间戳日志），再做凭据预检（本用例无凭据 → 致命）
    let r = run_hostbee(
        &["daemon", "--interval", "604800", "--foreground"],
        &[
            ("HOME", home_str.as_str()),
            ("HOSTBEE_ENDPOINT", "http://127.0.0.1:1"),
        ],
    );
    assert_eq!(r.code, Some(1));
    let lines: Vec<&str> = r.stderr.lines().collect();
    assert!(lines.len() >= 2, "应有警告日志 + JSON 错误: {lines:?}");
    assert!(lines[0].contains("7 天"), "首行应为 TTL 警告: {}", lines[0]);
    assert!(
        lines[0].starts_with('['),
        "日志行应带时间戳前缀: {}",
        lines[0]
    );
    // 最后一行是一行 JSON 错误
    let json: Value = serde_json::from_str(lines[lines.len() - 1]).expect("末行应为 JSON 错误");
    let message = json["errors"][0]["message"].as_str().unwrap();
    assert!(message.contains("凭据"), "应提示凭据缺失: {message}");
}
// ========== codegen 命令面（vm 领域 tracer，ticket #4） ==========

use hostbee::generated::FIELDS;

/// 注册表条目（e2e 直接引用生成产物常量，避免测试内复制 document 文本）。
fn spec(command: &str) -> &'static hostbee::commands::FieldSpec {
    FIELDS
        .iter()
        .find(|f| f.domain == "vm" && f.command == command)
        .unwrap_or_else(|| panic!("注册表应有 vm/{command}"))
}

#[test]
fn vm_查询_分页flags与_json_透传() {
    let stub = GraphqlStub::echo();
    let r = run_hostbee(
        &[
            "vm",
            "vm-instances",
            "--page-size",
            "1",
            "--page-num",
            "2",
            "--filter",
            r#"{"hostname":"web-1"}"#,
            "--endpoint",
            stub.url(),
        ],
        &[],
    );
    assert_eq!(r.code, Some(0), "stderr: {}", r.stderr);
    assert_eq!(r.stderr, "");
    // 回显模式：data 里是 stub 收到的 query/variables，两侧都断言
    let echoed: Value = serde_json::from_str(r.stdout.trim_end()).unwrap();
    assert_eq!(echoed["query"], spec("vm-instances").documents[3]);
    assert_eq!(
        echoed["variables"],
        serde_json::json!({"pageSize": 1, "pageNum": 2, "filter": {"hostname": "web-1"}})
    );
}

#[test]
fn vm_list_别名_默认参数可跑() {
    let stub = GraphqlStub::echo();
    // `vm list`（手写别名）+ 分页参数缺省：CLI 默认 25/1/{}，无参可跑
    let r = run_hostbee(&["vm", "list", "--endpoint", stub.url()], &[]);
    assert_eq!(r.code, Some(0), "stderr: {}", r.stderr);
    let echoed: Value = serde_json::from_str(r.stdout.trim_end()).unwrap();
    assert_eq!(echoed["query"], spec("vm-instances").documents[3]);
    assert_eq!(
        echoed["variables"],
        serde_json::json!({"pageSize": 25, "pageNum": 1, "filter": {}})
    );
}

#[test]
fn vm_查询_depth_选档与_fields_覆盖() {
    let stub = GraphqlStub::echo();
    let r = run_hostbee(
        &[
            "vm",
            "vm-instances",
            "--depth",
            "0",
            "--endpoint",
            stub.url(),
        ],
        &[],
    );
    assert_eq!(r.code, Some(0), "stderr: {}", r.stderr);
    let echoed: Value = serde_json::from_str(r.stdout.trim_end()).unwrap();
    assert_eq!(echoed["query"], spec("vm-instances").documents[0]);

    let r = run_hostbee(
        &[
            "vm",
            "vm-instances",
            "--fields",
            "{ nodes { id status } totalNum }",
            "--endpoint",
            stub.url(),
        ],
        &[],
    );
    assert_eq!(r.code, Some(0), "stderr: {}", r.stderr);
    let echoed: Value = serde_json::from_str(r.stdout.trim_end()).unwrap();
    assert_eq!(
        echoed["query"],
        format!(
            "{} {{ nodes {{ id status }} totalNum }} }}",
            spec("vm-instances").prefix
        )
    );
}

#[test]
fn vm_mutation_json_透传() {
    let stub = GraphqlStub::echo();
    let r = run_hostbee(
        &[
            "vm",
            "update-vm-instance",
            "--input",
            r#"{"id":608,"rootPassword":"new-pw"}"#,
            "--endpoint",
            stub.url(),
        ],
        &[],
    );
    assert_eq!(r.code, Some(0), "stderr: {}", r.stderr);
    let echoed: Value = serde_json::from_str(r.stdout.trim_end()).unwrap();
    assert_eq!(echoed["query"], spec("update-vm-instance").documents[3]);
    // 必填输入对象整体 JSON 透传进 variables
    assert_eq!(
        echoed["variables"],
        serde_json::json!({"input": {"id": 608, "rootPassword": "new-pw"}})
    );
}

#[test]
fn vm_命令_自动携带_hb_auth() {
    let stub = GraphqlStub::canned(200, r#"{"data":{"vmInstances":{"totalNum":0}}}"#);
    let home = TempDir::new().unwrap();
    let home_str = write_config(
        &home,
        &format!(
            "endpoint = \"{}\"\naccess_token = \"acc-e2e\"\n",
            stub.url()
        ),
    );
    let r = run_hostbee(&["vm", "vm-instances"], &[("HOME", home_str.as_str())]);
    assert_eq!(r.code, Some(0), "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "{\"vmInstances\":{\"totalNum\":0}}\n");
    let requests = stub.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].hb_auth.as_deref(), Some("acc-e2e"));
}

#[test]
fn vm_命令_graphql_错误进_stderr_exit_非0() {
    let stub = GraphqlStub::canned(
        400,
        r#"{"data":null,"errors":[{"message":"vmId 不存在","extensions":{"code":"vm.not_found"}}]}"#,
    );
    let r = run_hostbee(
        &[
            "vm",
            "vm-instance-by-subscription",
            "--subscription-id",
            "99999",
            "--endpoint",
            stub.url(),
        ],
        &[],
    );
    assert_eq!(r.code, Some(1));
    assert_eq!(r.stdout, "");
    let json = r.stderr_json();
    assert_eq!(json["errors"][0]["message"], "vmId 不存在");
    assert_eq!(json["errors"][0]["extensions"]["code"], "vm.not_found");
}

#[test]
fn vm_组_help_输出全命令面() {
    let r = run_hostbee(&["vm", "--help"], &[]);
    assert_eq!(r.code, Some(0));
    // 组 help：命令面 + 别名可见（前缀发现）
    assert!(r.stdout.contains("vm-instances"));
    assert!(r.stdout.contains("vm-power-action"));
    assert!(r.stdout.contains("[alias: list]"));
    // 叶子 help：参数 flags 与运行时 flag（--depth/--fields/--endpoint）可见
    let r = run_hostbee(&["vm", "vm-instances", "--help"], &[]);
    assert_eq!(r.code, Some(0));
    assert!(r.stdout.contains("--page-size"));
    assert!(r.stdout.contains("--filter"));
    assert!(r.stdout.contains("--depth"));
    assert!(r.stdout.contains("--fields"));
    assert!(r.stdout.contains("--endpoint"));
}

#[test]
fn 参数错误统一_json_而帮助版本成功() {
    for args in [
        vec![],
        vec!["unknown"],
        vec!["vm", "--unknown"],
        vec!["vm", "vm-init"],
        vec!["vm", "vm-instances", "--depth", "9"],
        vec!["vm", "vm-instances", "--page-size", "bad"],
    ] {
        let r = run_hostbee(&args, &[]);
        assert_eq!(r.code, Some(1), "{args:?}: {}", r.stderr);
        assert!(r.stdout.is_empty());
        assert!(r.stderr_json()["errors"].is_array());
    }
    for args in [vec!["--help"], vec!["--version"], vec!["vm", "--help"]] {
        let r = run_hostbee(&args, &[]);
        assert_eq!(r.code, Some(0));
        assert!(r.stderr.is_empty());
        assert!(!r.stdout.is_empty());
    }
}

fn captcha_required() -> (u16, String) {
    (
        400,
        r#"{"errors":[{"message":"需要验证码 ID","extensions":{"code":"captcha.id_required"}}]}"#
            .into(),
    )
}

fn captcha_image() -> (u16, String) {
    let pixels = image::RgbImage::from_pixel(200, 60, image::Rgb([255, 255, 255]));
    let mut jpeg = std::io::Cursor::new(Vec::new());
    pixels
        .write_to(&mut jpeg, image::ImageFormat::Jpeg)
        .unwrap();
    (200, serde_json::json!({"data":{"generateCaptcha":{"captchaId":"challenge", "imageBase64":format!("data:image/jpeg;base64,{}", data_encoding::BASE64.encode(jpeg.get_ref()))}}}).to_string())
}

#[test]
fn login_captcha_图片降级并携带凭证完成登录() {
    for totp in [true, false] {
        let mut replies = vec![captcha_required(), captcha_image(),
            (200, r#"{"data":{"verifyCaptcha":{"captchaId":"verified"}}}"#.into()),
            (200, serde_json::json!({"data":{"login":{"accessToken":"login-token","userInfo":{"hasTotp":totp}}}}).to_string()),
        ];
        let field = if totp {
            "verifyTotp"
        } else {
            replies.push((200, r#"{"data":{"sendVerificationCode":true}}"#.into()));
            "verifyVerificationCode"
        };
        replies.push((200, serde_json::json!({"data":{field:{"accessToken":"access","refreshToken":"refresh","userInfo":{"hasTotp":totp}}}}).to_string()));
        let stub = GraphqlStub::scripted(replies);
        let home = TempDir::new().unwrap();
        let mut args = vec![
            "login",
            "--endpoint",
            stub.url(),
            "--contact",
            STUB_CONTACT,
            "--password",
            STUB_PASSWORD,
        ];
        if totp {
            args.extend(["--totp-secret", TOTP_BASE32]);
        }
        let r = run_hostbee_input(
            &args,
            &[
                ("HOME", home.path().to_str().unwrap()),
                ("TERM", "xterm-kitty"),
            ],
            "ABCDE\n123456\n",
        );
        assert_eq!(r.code, Some(0), "{}", r.stderr);
        assert_eq!(
            serde_json::from_str::<Value>(&r.stdout).unwrap()["refreshToken"],
            "refresh"
        );
        assert!(r.stderr.contains("验证码图片："));
        assert!(
            !r.stderr.contains("\x1b_G"),
            "重定向 stderr 不得输出 Kitty 控制码"
        );
        let requests = stub.requests();
        assert_eq!(requests.len(), if totp { 5 } else { 6 });
        for (index, req) in requests.iter().enumerate() {
            assert_eq!(
                req.captcha_id.as_deref(),
                if index == 3 { Some("verified") } else { None }
            );
        }
        assert_eq!(
            serde_json::from_str::<Value>(&requests[2].body).unwrap()["variables"]["input"],
            serde_json::json!({"captchaId":"challenge","answer":"ABCDE"})
        );
        assert_eq!(
            read_config(home.path().to_str().unwrap())
                .refresh_token
                .as_deref(),
            Some("refresh")
        );
    }
}

#[test]
fn login_captcha_错误过期及输入中断不覆盖凭据() {
    for mode in ["wrong", "expired", "eof", "empty", "generate", "missing"] {
        let mut replies = vec![captcha_required()];
        if mode == "generate" {
            replies.push((429, r#"{"errors":[{"message":"限流","extensions":{"code":"CAPTCHA_RATE_LIMIT_EXCEEDED"}}]}"#.into()));
        } else {
            replies.push(captcha_image());
            if mode == "wrong" {
                replies.push((403, r#"{"errors":[{"message":"答案错误","extensions":{"code":"CAPTCHA_VERIFICATION_FAILED"}}]}"#.into()));
            } else if mode == "missing" {
                replies.push((200, r#"{"data":{"verifyCaptcha":{"captchaId":""}}}"#.into()));
            } else if mode == "expired" {
                replies.push((
                    200,
                    r#"{"data":{"verifyCaptcha":{"captchaId":"verified"}}}"#.into(),
                ));
                replies.push((400, r#"{"errors":[{"message":"凭证过期","extensions":{"code":"captcha.invalid_or_expired"}}]}"#.into()));
            }
        }
        let stub = GraphqlStub::scripted(replies);
        let home = TempDir::new().unwrap();
        let path = home.path().join(".hostbee/config.toml");
        config::write_config_atomic(
            &path,
            &Config {
                access_token: Some("keep-access".into()),
                refresh_token: Some("keep-refresh".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let before = std::fs::read(&path).unwrap();
        let input = match mode {
            "eof" => "",
            "empty" => "\n",
            _ => "ABCDE\n",
        };
        let r = run_hostbee_input(
            &[
                "login",
                "--endpoint",
                stub.url(),
                "--contact",
                STUB_CONTACT,
                "--password",
                STUB_PASSWORD,
            ],
            &[("HOME", home.path().to_str().unwrap())],
            input,
        );
        assert_eq!(r.code, Some(1), "{mode}: {}", r.stderr);
        assert!(r.stdout.is_empty());
        let last = r.stderr.lines().last().unwrap();
        assert!(serde_json::from_str::<Value>(last).unwrap()["errors"].is_array());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(
            stub.requests().len(),
            match mode {
                "expired" => 4,
                "wrong" | "missing" => 3,
                _ => 2,
            }
        );
    }
}

#[test]
fn 自动重登遇到_captcha_直接报错不交互() {
    let stub = GraphqlStub::scripted(vec![
        (
            400,
            r#"{"errors":[{"message":"未授权","extensions":{"status":401}}]}"#.into(),
        ),
        captcha_required(),
    ]);
    let home = TempDir::new().unwrap();
    let path = home.path().join(".hostbee/config.toml");
    config::write_config_atomic(
        &path,
        &Config {
            endpoint: Some(stub.url().into()),
            contact: Some(STUB_CONTACT.into()),
            password: Some(STUB_PASSWORD.into()),
            access_token: Some("expired".into()),
            totp_secret: Some(TOTP_BASE32.into()),
            ..Default::default()
        },
    )
    .unwrap();
    let before = std::fs::read(&path).unwrap();
    let r = run_hostbee(
        &["gql", "{ __typename }"],
        &[("HOME", home.path().to_str().unwrap())],
    );
    assert_eq!(r.code, Some(1));
    assert!(r.stdout.is_empty());
    assert!(r.stderr_json()["errors"].is_array());
    assert_eq!(stub.requests().len(), 2);
    assert_eq!(attempt_count(&stub, "generateCaptcha"), 0);
    assert_eq!(std::fs::read(path).unwrap(), before);
}
