//! hostbee：rustybee 后端（GraphQL）的 agent 专用 CLI。
//!
//! 实现逻辑放在 lib 中：unit 测试直接测这些模块，二进制入口（`src/main.rs`）只做
//! 参数解析与输入输出编排。后续 codegen crate（ticket #4）生成的命令面也复用这里的
//! endpoint 解析、配置读写与认证生命周期（[`auth::execute`]）。

pub mod auth;
pub mod config;
pub mod endpoint;
pub mod error;
pub mod gql;
