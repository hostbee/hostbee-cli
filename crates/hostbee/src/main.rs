//! hostbee 二进制入口：参数解析 + 输入输出编排。
//!
//! 输出契约：stdout 仅打印结果 JSON（compact 单行）；所有错误打印到
//! stderr（一行 `{"errors":[...]}` JSON）并以 exit code 1 退出。
//! 诊断性警告（如配置写盘失败）也走 stderr 纯文本，stdout 永远只有结果 JSON。
//!
//! 命令树构建与分发见 [`hostbee::commands`]：手写命令（login/gql）+
//! codegen 领域组（[`hostbee::generated`]，[`hostbee::commands::ENABLED_DOMAINS`]
//! 控制接线范围）。使用 clap builder API 而非巨型 derive：210 命令面下
//! 编译时间与启动延迟都更稳（codegen-design.md §6）。

use std::process::ExitCode;

use hostbee::commands;

fn main() -> ExitCode {
    let matches = match commands::build_cli().try_get_matches() {
        Ok(matches) => matches,
        Err(err)
            if matches!(
                err.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) =>
        {
            print!("{err}");
            return ExitCode::SUCCESS;
        }
        Err(err) => {
            eprintln!(
                "{}",
                hostbee::error::CliError::Input(err.to_string()).stderr_json()
            );
            return ExitCode::from(hostbee::error::FAILURE_EXIT_CODE);
        }
    };
    let result = match matches.subcommand() {
        // daemon 无 stdout JSON 契约（stdout 恒空、日志全在 stderr），单独编排
        Some(("daemon", sub)) => {
            return hostbee::daemon::main_entry(
                sub.get_one::<u64>("interval")
                    .copied()
                    .unwrap_or(hostbee::daemon::DEFAULT_INTERVAL_SECS),
                sub.get_flag("foreground"),
                sub.get_one::<String>("endpoint").map(String::as_str),
            );
        }
        Some(("login", sub)) => commands::login_cmd(sub),
        Some(("gql", sub)) => commands::gql_cmd(sub),
        Some((domain, sub)) => commands::dispatch_generated(domain, sub),
        _ => {
            // arg_required_else_help(true)：无参数视为输入错误（exit 1，stderr JSON 内含 help）
            unreachable!("无子命令时 clap 直接退出，不会进入 main 分发")
        }
    };
    match result {
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
            ExitCode::from(hostbee::error::FAILURE_EXIT_CODE)
        }
    }
}
