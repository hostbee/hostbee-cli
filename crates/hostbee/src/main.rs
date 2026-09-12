//! hostbee 二进制入口：参数解析 + 输入输出编排。
//!
//! 输出契约：stdout 仅打印查询结果 JSON（compact 单行）；所有错误打印到
//! stderr（一行 `{"errors":[...]}` JSON）并以 exit code 1 退出。

use std::process::ExitCode;

use clap::{Parser, Subcommand};
use hostbee::endpoint::{self, ENDPOINT_ENV};
use hostbee::error::{CliError, FAILURE_EXIT_CODE};
use hostbee::gql;
use serde_json::Value;

/// rustybee 后端的 agent 专用 CLI。命令面由 schema codegen 生成（ticket #4），
/// `gql` 为透传任意 GraphQL document 的逃生门。
#[derive(Parser)]
#[command(name = "hostbee", version, about, disable_help_subcommand = true)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// gql 逃生门：任意 GraphQL document 原样透传到 <endpoint>/graphql
    Gql {
        /// GraphQL document（query/mutation），原样透传，不做任何改写
        document: String,
        /// 可选 variables，JSON 字符串，如 '{"id":1}'
        #[arg(long)]
        variables: Option<String>,
        /// 覆盖 endpoint（优先级：flag > HOSTBEE_ENDPOINT > ~/.hostbee/config.toml）
        #[arg(long)]
        endpoint: Option<String>,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(data) => {
            // compact 单行 JSON：效率优先，stdout 不掺杂任何其他输出。
            println!(
                "{}",
                serde_json::to_string(&data).expect("Value 序列化不会失败")
            );
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("{}", err.stderr_json());
            ExitCode::from(FAILURE_EXIT_CODE)
        }
    }
}

fn run(cli: Cli) -> Result<Value, CliError> {
    let Command::Gql {
        document,
        variables,
        endpoint: endpoint_flag,
    } = cli.command;
    let variables = variables.as_deref().map(gql::parse_variables).transpose()?;
    let config = match endpoint::default_config_path() {
        Some(path) => endpoint::read_config_endpoint(&path),
        None => Ok(None),
    };
    let resolved = endpoint::resolve_endpoint(
        endpoint_flag.as_deref(),
        std::env::var(ENDPOINT_ENV).ok(),
        config,
    )?;
    gql::execute(&resolved, &document, variables)
}
