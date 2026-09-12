# hostbee-cli

rustybee 后端（GraphQL over axum）的 CLI 客户端，用 Rust 编写。唯一消费者是持有 admin
账号、在安全内网长期自主运行的 agent：stdout 只输出查询结果纯 JSON，凭据明文存储，
效率是第一优先级。项目背景与术语见 [CONTEXT.md](CONTEXT.md)，关键决策见
[docs/adr/](docs/adr/)。

## 构建

```sh
cargo build --release          # 产物：target/release/hostbee
cargo build                    # 开发构建：target/debug/hostbee
```

`Cargo.lock` 随仓库提交（二进制 crate，保证可复现构建）。

## gql 逃生门

命令面将按 `vendor/backend/rustybee/schema.graphql` 全量 codegen 生成（ticket #4）；
在 codegen 覆盖不到的窗口期，用 `gql` 逃生门透传任意 GraphQL document：

```sh
hostbee gql '{ backendVersion { version commitHash buildTime } }' --endpoint http://127.0.0.1:8000

# variables 为可选 JSON 字符串
hostbee gql 'query Q($id: Int!) { vmInstance(id: $id) { id name } }' \
  --variables '{"id":1}' --endpoint http://127.0.0.1:8000
```

document 原样 POST 到 `<endpoint>/graphql`（body 为 `{"query": doc, "variables": ...}`），
不做任何改写。

### endpoint 解析顺序

`--endpoint` flag → `HOSTBEE_ENDPOINT` 环境变量 → `~/.hostbee/config.toml` 中的
`endpoint` 键（本仓库当前只读取该文件；凭据持久化写入由 login 流程负责，见 ticket #3）。
三者全缺时直接失败，不会发起请求。

## 输出契约

| 通道 | 成功 | 失败 |
| --- | --- | --- |
| stdout | 仅一行 compact JSON（GraphQL envelope 中 `data` 的值） | 无输出 |
| stderr | 无输出 | 仅一行 JSON：`{"errors":[...]}` |
| exit code | `0` | `1` |

- 服务端返回的 GraphQL `errors` 数组**原样**透传到 stderr（含 `locations`、`extensions`
  等任意字段）。本地后端的校验错误走 HTTP 400 + 带 `errors` 的 envelope，同样按此透传。
- 传输失败、非 2xx 无 errors envelope、配置/参数错误等本地失败，合成同形状的
  `{"errors":[{"message":"..."}]}`。
- 失败统一 exit code `1`，失败原因一律从 stderr JSON 读取，不再细分码位。
- 日志与诊断一律走 stderr，stdout 永远只有结果 JSON，可安全管道给 `jq`。

## HTTP 客户端选型

选用 [`ureq`](https://docs.rs/ureq)（同步、阻塞式），对比 `reqwest`（blocking）：

- **启动延迟**：ureq 依赖树极小（无 tokio、无 hyper、无 openssl），二进制更小、编译更快，
  符合「二进制启动到出结果的延迟尽可能低」的 spec 优先级；agent 高频调用场景收益直接。
- **TLS**：默认 rustls + ring provider，不链接 openssl / cmake（`default-features = false`
  并开启 `json`、`rustls`，去掉不需要的 gzip 等特性进一步缩减依赖）。
- 后端是纯查询/变更的轮询模型（无 GraphQL subscriptions），不需要异步并发能力。

edition 选用 **2024**（当前 stable edition，rustc 1.85+）：greenfield 项目没有历史包袱，
直接落在最新语言基线上；workspace 使用 `resolver = "3"`。

## 质量门禁（pre-push hook）

仓库无远端 CI，质量门禁由 git hook 在 push 前强制执行。一次性启用：

```sh
git config core.hooksPath .githooks
```

hook（[.githooks/pre-push](.githooks/pre-push)）依次运行，任一失败即中止 push：

1. `cargo fmt --check`
2. `cargo clippy --workspace --all-targets -- -D warnings`
3. `cargo test --workspace`（unit + e2e）

e2e 测试完全 hermetic（内存 stub server），hook 不访问 localhost 以外的网络。
**Agent 禁止以任何方式跳过 hook 完成 push**（`--no-verify`、`HUSKY=0`、卸载 hook 等，
见 AGENTS.md）；hook 失败时修复问题后重新 push。

## 测试

- **unit**（`cargo test --lib`）：endpoint 解析优先级、配置文件读取、GraphQL envelope
  分类、错误 JSON 格式化等纯逻辑，与实现同文件放在 `#[cfg(test)]` 模块。
- **e2e**（`crates/hostbee/tests/e2e/`，`cargo test --test e2e`）：全仓库唯一的「高 seam」，
  进程边界测试。测试内起内存 stub GraphQL HTTP server（tiny_http，dev-dep），spawn
  编译好的真实二进制（`env!("CARGO_BIN_EXE_hostbee")`）指向它，断言 stdout JSON、
  exit code、stderr 错误 JSON 三通道；覆盖成功、GraphQL errors（含 400 + envelope）、
  传输失败、endpoint 各来源与优先级、variables 透传与非法 JSON 等路径。
  子进程 `env_clear` 且 HOME 指向临时目录，与真实 `~/.hostbee/` 完全隔离。

## 仓库结构

```
Cargo.toml               # workspace 根（resolver = 3）
crates/hostbee/          # 唯一成员：hostbee CLI（lib + bin）
  src/
    lib.rs               # 模块入口（endpoint / error / gql）
    main.rs              # 二进制入口：clap 解析 + stdin/stdout 编排
  tests/e2e/             # e2e harness（stub server + 三通道断言）
.githooks/pre-push       # 质量门禁 hook
vendor/                  # 后端与 webclient 子模块（只读参考，禁止修改）
```

后续 codegen crate（schema.graphql → 命令面生成器，ticket #4）将作为新成员加入
`crates/`；登录与凭据持久化（ticket #3）、daemon 保活（ticket #6）在 `hostbee`
crate 内扩展。