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

## codegen 命令面

命令面按 `vendor/backend/rustybee/schema.graphql` 全量生成：210 个 root field →
按领域分组的两层子命令（Query/Mutation 混排），参数 → flags，结果与 GraphQL 返回
JSON 同构。设计决策见 [docs/adr/0002-命令面全量-codegen.md](docs/adr/0002-命令面全量-codegen.md)。

```sh
hostbee --help                            # 全量命令面：18 领域组 + login/gql/daemon
hostbee vm --help                         # 领域组内前缀发现全部命令
hostbee vm vm-instances --page-size 1   # 分页查询：默认 25/1/filter {}
hostbee order orders-paging --page-num 2  # 任意领域同理（order/subscription/store/…）
hostbee vm list                           # 手写别名 → vm vm-instances
hostbee vm update-vm-instance --input '{"id":608,"rootPassword":"x"}'
                                        # 输入对象整体 JSON 透传
hostbee vm vm-instances --depth 8      # selection set 展开深度 0..=8，默认 3
hostbee vm vm-instances --fields '{ nodes { id status } totalNum }'
                                        # 完全覆盖生成的 selection set
```

- **接线范围**：全 18 个领域组全部接线（ticket #5），`hostbee --help` 即全量发现面
  （spec 用户故事 11/12）。生成器产物覆盖全部 210 个 root field，`login`/`refresh`
  两个 field 由 auth.rs 手写实现（codegen 让位，`hostbee login` 直属根命令）；
  admin/mgmt 操作与用户操作同一扁平命令面（ADR-0001，不设权限分层）。
- **分页**：默认单页。`pageSize/pageNum` 风格 CLI 默认 `--page-size 25`/`--page-num 1`；
  游标风格 CLI 默认 `--first 25`（`--after`/`--before`/`--last` 可选）。输出保留
  schema 中的分页元数据（`totalNum` / `pageInfo` / `totalCount`）。
- **全量列表命令的体积注意**：schema 中无服务端分页的全量列表 field（`orders`、
  `subscriptions` 等）在数据量大的后端上按默认 depth 3 展开可能超过 HTTP 传输的
  10MB 响应上限（实测 6611 个订单的 `orders` 超限报错）。此类场景用分页变体
  （`orders-paging` 等）或 `--depth 0`（标量展开，实测 1.8MB 可跑）。
- **插件 mutation 的 BorshOrJson**：`plugin plugin-mutation-resource-*` 等命令的
  input 是 oneOf（borsh/json 二选一），CLI 不自动包装——json 分支传 `--input '{"json":{}}'`，
  整个 oneOf 输入原样 JSON 透传（codegen-design.md §3；sdc 网络数据面与插件内部机制不设专用命令，
  spec #1 Out of Scope）。
- **产物再生成**：同域合法字段新增后仅需运行 `cargo run -p hostbee-codegen`（不限制历史字段数量；未知域或类型仍 fail-fast），产物
  （`crates/hostbee/src/generated/{mod.rs,docs.rs}`）commit 进仓库，重跑幂等。
  产物头部带 schema sha256 漂移标记，`cargo test` 会校验并提示重跑。
  **前提**：`vendor/backend/rustybee` 子模块须初始化（`git submodule update --init`）。
- 手写别名表（`vm list`、`vm search`、`order list`→orders-paging）在 `commands.rs`
  的 `ALIASES`，不进 codegen 规则；schema 演进引入 flag 撞名（参数 kebab 化后重复、
  或与运行时 `--depth`/`--fields`/`--endpoint` 撞名）时生成器 fail-fast。

## gql 逃生门

schema 演进快于本仓库发布时的兜底：任意 GraphQL document 原样透传，

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
`endpoint` 键。三者全缺时直接失败，不会发起请求。

## 认证与凭据

### 登录

```sh
hostbee login --endpoint http://127.0.0.1:8000 \
  --contact 'user@example.com' --password 'secret' --totp-secret '<Base32>'
# flags 缺省时交互提示；HOSTBEE_CONTACT / HOSTBEE_PASSWORD / HOSTBEE_TOTP_SECRET 亦可
```

- 登录后凭据**明文**持久化到 `~/.hostbee/config.toml`（ADR-0001：内网 agent 专用，
  不做任何加密/密钥环/警告）。文件不存在时自动创建。
- **邮件验证码登录**：直接运行 `hostbee login`，输入 contact 和 password 后，CLI
  发送邮件并提示输入验证码，通过 `verifyVerificationCode` 换取 access + refresh token 对。
  启用或未启用 TOTP 的账号均支持；验证码只从 stdin 读取，不保存到配置文件。
- **TOTP 自动验证**：账号启用 TOTP 且提供了 `totp_secret` 时，本地生成验证码
  （RFC 6238 SHA-256）并调用 `verifyTotp`，不发送邮件。secret 无效或验证失败则报错。
- **完成条件**：只有取得完整 token 对才保存登录凭据；密码登录已返回完整 token 对时
  直接成功。邮件发送失败、验证码错误／过期、空输入或 EOF 均失败退出，保留已有配置。
  交互提示走 stderr，stdout 成功时仍只输出一行 AuthOutput JSON。

### 登录时的图形验证码（CAPTCHA）

后端返回 `captcha.id_required` 时，`hostbee login` 自动获取验证码图片，打印临时
图片的绝对路径，等待人工输入答案；验证成功后携带 `X-CAPTCHA-ID` 重试登录一次，
再继续 TOTP 或邮件验证。无需额外参数，不能用 TOTP 代替图形验证码。

stderr 为终端且 `TERM=xterm-kitty` 或存在 `KITTY_WINDOW_ID` 时，使用 Kitty 图形
协议显示图片；处于 tmux/screen 或 stderr 重定向时只输出图片路径。图片始终落盘，
显示失败也可手动打开；SSH 场景的图片位于远端，需自行取回查看。临时图片在此次
验证码交互结束后清理。后端验证码有效期为 5 分钟，答案错误、凭证过期、空输入、
EOF 或服务端限流均报错退出，重新运行 `hostbee login` 可重新获取图片。

图片与提示写 stderr，stdout 保持 JSON；失败时原有配置不变。自动重登与 daemon
不触发验证码交互：被 CAPTCHA 拦截时保留错误，由用户手动运行 `hostbee login`。

TOTP 密钥直接填写 `otpauth://` 链接中 `secret` 参数的 **Base32** 值，不能填写整个链接。
接受大小写、合法的尾部 `=` padding 或无 padding，并忽略首尾空白。
配置字段、`HOSTBEE_TOTP_SECRET` 和 `--totp-secret` 使用相同规则。

**迁移：不再支持 hex。** 已有 hex 配置必须替换为原始 Base32 密钥；不自动转换。
部分 hex 字符串也符合 Base32 语法，会按 Base32 解码并产生不同验证码，不能依赖报错识别旧配置。

### 配置文件布局

```toml
endpoint = "http://127.0.0.1:8000"
contact = "user@example.com"
password = "secret"                 # 密码重登兜底用
totp_secret = "<Base32>"               # 可选；TOTP 账号自动交换用
access_token = "<jwt>"
refresh_token = "<jwt>"
```

写入全部走**原子写**（同目录临时文件 + fsync + rename），refresh 轮换落盘不会
出现半更新状态。

### token 生命周期与自动恢复边界

每次 GraphQL 调用自动携带 `HB-AUTH: Bearer <accessToken>`：

1. 收到认证失败（后端形状：HTTP 200/400 + `errors[].extensions.status == 401`，
   HTTP 401 一并兼容）→ 用 refreshToken 调 `refresh` 轮换（后端会轮换两个 token
   并撤销旧 refreshToken）→ 原子落盘 → **重试原请求一次**。
2. refresh 也失败（token 撤销/过期，后端形状为 HTTP 500 `messages.internal_error`）
   → 用存储的 contact + password 重新 login（TOTP 账号自动完成 verifyTotp 交换）
   → 取得完整 token 对后原子落盘 → 再重试一次。
   没有自动验证条件时，提示运行 `hostbee login` 完成邮件验证；普通查询不发邮件、不等待输入。
3. 全部失败：exit code 非 0，stderr 一行 JSON（`errors` 含原始错误 + 恢复过程附注）。

环境变量 `HOSTBEE_ENDPOINT`、`HOSTBEE_REFRESH_TOKEN` 覆盖配置文件；env 提供的
refreshToken 失效时自动回退到配置文件中的值兜底。

> **密码重登前提**：后端站点设置 `siteCaptchaEnabledForLogin` 须为关（agent 负责
> 确认；本地 dev 后端可经 `siteGlobalSettingsAlter` 或直接 SQL 调整）。设置开启时
> 密码登录需验证码，后端**不把验证码文本写进任何日志**，无人工介入的自动重登不成立。

## daemon 保活（`hostbee daemon`）

后端 refreshToken 为 **7 天固定 TTL、无滑动续期**（`hivelib-services/src/service/jwt.rs`），
闲置 7 天后 CLI 需要重新登录；有 TOTP secret 时可自动重登，否则需要交互邮件验证。daemon 用远小于 TTL 的周期（默认 `--interval 86400`
= 每天一次）主动调 `refresh` 轮换并把新 token 对**原子落盘**，消除「闲置过期」；
CLI 侧 401 → refresh → 密码重登的兜底路径保持不变（daemon 只消除闲置过期，不替代兜底）。

### 运行语义（install vs foreground）

| 场景 | 行为 |
| --- | --- |
| systemd 机器，`hostbee daemon` | 写 user unit → `systemctl --user daemon-reload` → `enable --now` → `try-restart` → 以 `is-active` 确认在跑 → stderr 提示后 **exit 0** |
| systemd 机器，安装/启动失败 | 明确 stderr 日志后**降级前台循环**（服务确认 active 时不会双跑） |
| 非 systemd（macOS/容器），`hostbee daemon` | 一条提示 `未检测到 systemd，前台保活运行；Ctrl-C 退出` 后直接前台循环 |
| 任意环境，`--foreground` | 不走安装，直接前台循环——unit 的 ExecStart 即此形态，服务进程不会递归安装（防 fork-bomb） |

- **unit 文件**：`~/.config/systemd/user/hostbee-keepalive.service`（`XDG_CONFIG_HOME`
  优先）。内容（golden，随 `--interval` 变化）：

  ```ini
  # 由 hostbee daemon 自动生成；改动周期后重新运行 hostbee daemon 即可更新。
  [Unit]
  Description=hostbee keepalive——refreshToken 定期轮换
  Wants=network-online.target
  After=network-online.target

  [Service]
  Type=simple
  ExecStart="<current_exe 绝对路径>" daemon --foreground --interval 86400
  Restart=always
  RestartSec=10s

  [Install]
  WantedBy=default.target
  ```

  - `ExecStart` 记录安装时的二进制绝对路径——**移动二进制后需重新运行 `hostbee daemon` 更新**；
  - 安全设置按 ADR-0001 保持最小，不引入沙箱/加固指令：unit 归用户自己所有，与 CLI
    进程同权限运行；
  - `Restart=always` 只兜底真实崩溃（OOM、panic）；循环自身已对瞬态错误退避，正常
    永不退出；凭据被清空时 daemon exit 1，journal 每 10s 一条明确报错（可见的运维信号）；
  - **linger**：user unit 的「开机自启无需登录」需要 linger，安装时尽力执行
    `loginctl enable-linger`，失败只提示不阻断（提示语含手动补救命令）。

- **幂等重装**：unit 内容未变时只 `enable --now`，不动运行中的服务；内容变化时才写盘
  + `daemon-reload` + `try-restart`（仅原本 active 时重启，让新 ExecStart 生效）。
- **systemd 服务读不到安装现场的 flag/env**：bare `hostbee daemon` 要求 endpoint 与
  凭据（refresh_token 或 contact+password）都在 `~/.hostbee/config.toml` 中——
  只有 env/flag 提供时明确报错退出，不安装一个必然起不来的服务。env-only 场景
  （CI、容器）请用 `--foreground`。

### 轮换循环

- 每轮**重新读盘**配置——配置文件是 daemon 与并发 CLI 的唯一共享事实，CLI 侧轮换后
  daemon 自动跟进新值；
- 依次尝试 refreshToken 候选（env > config，与 CLI 恢复路径同序），全部失败且存有
  contact+password 时密码重登（复用 CLI 的 login_full，含 TOTP 自动交换），daemon 不
  引入新恢复机制；
- 成功 → 原子落盘（与 CLI 同一套写路径）；密码重登未取得完整 token 对时按失败退避，
  提示运行 `hostbee login`，保留已有配置；daemon 不发送邮件、不读取 stdin。
- 首轮在启动时立即执行（重启即验证凭据可用，不等一个周期）。

### 周期与退避

- `--interval <secs>` 默认 86400；校验非 0，`interval >= 604800`（7 天 TTL）时启动
  打警告（建议显著调小）但仍可运行。
- **退避曲线**：连续失败从 `min(60s, interval)` 起指数加倍，封顶 `max(interval/4, 起始值)`，
  成功即复位。默认配置下 60s → 2m → 4m → … → 6h 封顶：
  - 封顶 interval/4 ⇒ 最坏重试节奏为每周期窗口 4 次，对失败中的后端压力有界；
  - 60s 起步让短瞬断在分钟级内被下一次重试覆盖，而 7 天 TTL 给足重试余量；
  - interval 本就小于 60s 时按其自身节奏退避（e2e/容器等小周期场景不被 60s 拖慢）。
- **HTTP 超时**：daemon 单次调用整体超时 30s——后端挂起时退避重试而不是卡死循环
  （CLI 单次命令不设超时，进程短生命周期语义不同）。

### 退出与日志

- **stdout 恒空**（daemon 无 stdout JSON 契约，可安全重定向）；全部日志走 stderr，
  每行 `[UTC RFC3339] 消息`；
- 致命错误（interval 为 0、缺 endpoint、无任何凭据）最后一行为一行 JSON
  `{"errors":[...]}` 并 exit 1，与 CLI 错误契约一致；
- SIGINT/SIGTERM：日志一行后**干净退出 exit 0**（systemd 停止服务、运维 Ctrl-C 同路径）；
- 唯一非 0 退出条件：无任何可保活凭据（无 refreshToken 且无 contact+password）。

## 输出契约

| 场景 | stdout | stderr / exit |
| --- | --- | --- |
| 普通调用成功 | 一行 compact data JSON | 无错误，exit 0 |
| 普通调用或参数失败 | 空 | 单行 errors JSON，exit 1 |
| 显式 --help / --version | 帮助或版本文本 | 空，exit 0 |
| 无参数调用 | 空 | 含使用帮助的单行 errors JSON，exit 1 |
| 交互登录 / 落盘警告 | 成功时仍为结果 JSON | 提示、诊断可为纯文本；落盘警告不改变请求成功状态 |
| daemon | 恒空 | 带时间戳日志；致命错误末行为 JSON，详见 daemon 章节 |

- 服务端返回的 GraphQL `errors` 数组**原样**透传到 stderr（含 `locations`、`extensions`
  等任意字段）。本地后端的校验错误走 HTTP 400 + 带 `errors` 的 envelope，同样按此透传。
- 传输失败、非 2xx 无 errors envelope、配置/参数错误等本地失败，合成同形状的
  `{"errors":[{"message":"..."}]}`。
- 失败统一 exit code `1`，普通调用失败原因从 stderr JSON 读取，不再细分码位。
- 普通业务调用的日志与诊断走 stderr，stdout 只有结果 JSON；显式帮助/版本输出文本，daemon 无 stdout。

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
  分类、错误 JSON 格式化、token 生命周期状态机（extensions 401 判定、重试边界、
  RFC 6238 TOTP 向量）、原子写语义等纯逻辑，与实现同文件放在 `#[cfg(test)]` 模块；
  daemon/systemd 侧含 interval 校验、退避曲线、UTC 时间戳向量、保活轮状态机
  （脚本化传输）、unit 文件 golden、systemd 检测/PATH 查找、假 systemctl 脚本驱动的
  完整安装流程（幂等重装、失败汇总）。
- **codegen 产物断言**（`crates/hostbee/tests/codegen_product.rs`）：schema sha256
  漂移标记（schema 变更后提示重跑生成器）、当前 schema 命令动态完整覆盖断言（领域全部接线、命令无重复）、全部九档 document `parse_query`
  可解析、`--fields` 拼接路径可解析、tracer（vmInstances）默认 document 逐字节
  快照、展开尺寸预算；`commands.rs` unit 侧断言全 18 领域组挂载且与注册表逐组
  一致（当前注册表命令全部可见）、别名逐条可见、注册表无 flag 冲突/撞名。
  生成器对 flag 冲突（参数 kebab 化后重复、与运行时 flag 撞名）fail-fast，
  含对应 unit 断言。
- **e2e**（`crates/hostbee/tests/e2e/`，`cargo test --test e2e`）：全仓库唯一的「高 seam」，
  进程边界测试。测试内起内存 stub GraphQL HTTP server（tiny_http，dev-dep），spawn
  编译好的真实二进制（`env!("CARGO_BIN_EXE_hostbee")`）指向它，断言 stdout JSON、
  exit code、stderr 错误 JSON 三通道；覆盖成功、GraphQL errors（含 400 + envelope）、
  传输失败、endpoint 各来源与优先级、variables 透传与非法 JSON，认证闭环
  （HB-AUTH 自动携带、access token 失效自动轮换重试、refresh 失效密码重登 + TOTP
  交换、env 覆盖与兜底、全失效原样报错），以及 daemon 保活（`--interval 1 --foreground`
  连续轮换 + 轮换期间 CLI 全程可用 + SIGTERM 干净退出；stub 故障开关驱动的退避日志
  与恢复自愈；非 systemd 环境的 bare daemon 降级前台循环；缺凭据 / interval 0 的
  致命退出契约）等路径。
  子进程 `env_clear` 且 HOME 指向临时目录，与真实 `~/.hostbee/` 完全隔离。

## 仓库结构

```
Cargo.toml               # workspace 根（resolver = 3）
crates/hostbee/          # CLI（lib + bin）
  src/
    lib.rs               # 模块入口（commands / generated / endpoint / error / gql / auth / config / daemon / systemd）
    main.rs              # 二进制入口：clap 解析 + stdin/stdout 编排
    commands.rs          # codegen 命令面运行时：注册表类型、clap 构建、--depth/--fields、执行接线
    generated/           # codegen 产物（commit 进仓库）：mod.rs 注册表 + docs.rs document 常量
    auth.rs              # token 生命周期状态机（401 判定 / refresh 轮换 / 密码重登 / TOTP）
    config.rs            # ~/.hostbee/config.toml 读写（原子写）
    daemon.rs            # daemon 保活循环（周期轮换 / 退避 / 信号退出 / systemd 安装编排）
    systemd.rs           # systemd user unit 生成与安装（检测 / PATH 查找 / systemctl 调用）
  tests/e2e/             # e2e harness（stub server + 三通道断言 + daemon 子进程场景）
  tests/codegen_product.rs  # 产物断言：sha256 漂移 / 结构 / parse_query / 快照
crates/hostbee-codegen/  # 生成器 bin：schema.graphql → generated/{mod,docs}.rs
.githooks/pre-push       # 质量门禁 hook
vendor/                  # 后端与 webclient 子模块（只读参考，禁止修改）
```

daemon 保活（ticket #6）在 `hostbee` crate 内扩展。
