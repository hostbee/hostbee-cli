//! hostbee：rustybee 后端（GraphQL）的 agent 专用 CLI。
//!
//! 实现逻辑放在 lib 中：unit 测试直接测这些模块，二进制入口（`src/main.rs`）只做
//! 参数解析与输入输出编排。命令面由 schema codegen 生成（[`generated`]），
//! 运行时接线见 [`commands`]；endpoint 解析、配置读写与认证生命周期
//! （[`auth::execute`]：401 → refresh 轮换 → 密码重登）为全命令面共用。

pub mod auth;
pub mod captcha;
pub mod commands;
pub mod config;
pub mod daemon;
pub mod endpoint;
pub mod error;
pub mod generated;
pub mod gql;
pub mod systemd;
