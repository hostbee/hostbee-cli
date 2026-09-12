//! endpoint 解析优先级：`--endpoint` flag → `HOSTBEE_ENDPOINT` 环境变量 → 配置文件。
//!
//! 配置文件的读取与写入见 [`crate::config`]（本模块只消费解析出的 endpoint 值）。

use crate::error::CliError;

/// endpoint 的环境变量覆盖。
pub const ENDPOINT_ENV: &str = "HOSTBEE_ENDPOINT";

/// 按优先级解析 endpoint：flag > env > 配置文件。
/// flag 与 env 为空字符串时视为未提供。
pub fn resolve_endpoint(
    flag: Option<&str>,
    env: Option<String>,
    config: Result<Option<String>, String>,
) -> Result<String, CliError> {
    if let Some(endpoint) = crate::config::non_empty(flag.map(str::to_owned)) {
        return Ok(endpoint);
    }
    if let Some(endpoint) = crate::config::non_empty(env) {
        return Ok(endpoint);
    }
    match config {
        Ok(Some(endpoint)) => crate::config::non_empty(Some(endpoint)).ok_or(CliError::NoEndpoint),
        Ok(None) => Err(CliError::NoEndpoint),
        Err(msg) => Err(CliError::Config(msg)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 优先级_flag_高于_env_与配置文件() {
        let config = Ok(Some("http://config:8000".to_owned()));
        let resolved = resolve_endpoint(
            Some("http://flag:8000"),
            Some("http://env:8000".to_owned()),
            config,
        );
        assert_eq!(resolved.unwrap(), "http://flag:8000");
    }

    #[test]
    fn 优先级_env_高于配置文件() {
        let config = Ok(Some("http://config:8000".to_owned()));
        let resolved = resolve_endpoint(None, Some("http://env:8000".to_owned()), config);
        assert_eq!(resolved.unwrap(), "http://env:8000");
    }

    #[test]
    fn 优先级_配置文件兜底() {
        let config = Ok(Some("http://config:8000".to_owned()));
        let resolved = resolve_endpoint(None, None, config);
        assert_eq!(resolved.unwrap(), "http://config:8000");
    }

    #[test]
    fn 全部缺失时报_未提供_endpoint() {
        assert!(matches!(
            resolve_endpoint(None, None, Ok(None)),
            Err(CliError::NoEndpoint)
        ));
    }

    #[test]
    fn 空_flag_与空_env_视为未提供() {
        let config = Ok(Some("http://config:8000".to_owned()));
        let resolved = resolve_endpoint(Some(""), Some(String::new()), config);
        assert_eq!(resolved.unwrap(), "http://config:8000");
    }

    #[test]
    fn 配置解析失败在无_flag_env_时上报() {
        let err = resolve_endpoint(None, None, Err("bad toml".to_owned())).unwrap_err();
        assert!(matches!(err, CliError::Config(ref msg) if msg.contains("bad toml")));
    }
}
