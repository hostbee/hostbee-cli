//! endpoint 解析：`--endpoint` flag → `HOSTBEE_ENDPOINT` 环境变量 → `~/.hostbee/config.toml`。
//!
//! 本模块只做读取；凭据持久化写入由 login 流程（ticket #3）负责。

use std::path::{Path, PathBuf};

use crate::error::CliError;

/// 配置文件所在目录（用户 home 下）。
pub const CONFIG_DIR: &str = ".hostbee";
/// 配置文件名。
pub const CONFIG_FILE: &str = "config.toml";
/// endpoint 的环境变量覆盖。
pub const ENDPOINT_ENV: &str = "HOSTBEE_ENDPOINT";

/// 当前用户的配置文件路径 `~/.hostbee/config.toml`；拿不到 home 目录时返回 None。
pub fn default_config_path() -> Option<PathBuf> {
    std::env::home_dir().map(|home| home.join(CONFIG_DIR).join(CONFIG_FILE))
}

/// 从配置文件读取 endpoint。文件不存在视为无配置（Ok(None)）；
/// 文件存在但读不了/解析不了是本地错误（Err）。
pub fn read_config_endpoint(path: &Path) -> Result<Option<String>, String> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("读取配置文件 {} 失败: {e}", path.display())),
    };
    // 注意：toml::Value 的 FromStr 解析的是单个 TOML 值（而非文档），完整文档要解析成 Table。
    let value: toml::Table = content
        .parse()
        .map_err(|e| format!("配置文件 {} 不是合法 TOML: {e}", path.display()))?;
    match value.get("endpoint") {
        None => Ok(None),
        Some(v) => v
            .as_str()
            .map(|s| Ok(Some(s.to_owned())))
            .unwrap_or_else(|| {
                Err(format!(
                    "配置文件 {} 的 endpoint 必须是字符串",
                    path.display()
                ))
            }),
    }
}

/// 按优先级解析 endpoint：flag > env > 配置文件。
/// flag 与 env 为空字符串时视为未提供。
pub fn resolve_endpoint(
    flag: Option<&str>,
    env: Option<String>,
    config: Result<Option<String>, String>,
) -> Result<String, CliError> {
    if let Some(endpoint) = non_empty(flag.map(str::to_owned)) {
        return Ok(endpoint);
    }
    if let Some(endpoint) = non_empty(env) {
        return Ok(endpoint);
    }
    match config {
        Ok(Some(endpoint)) => non_empty(Some(endpoint)).ok_or(CliError::NoEndpoint),
        Ok(None) => Err(CliError::NoEndpoint),
        Err(msg) => Err(CliError::Config(msg)),
    }
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_config(content: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        fs::write(&path, content).unwrap();
        (dir, path)
    }

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
    fn 配置文件解析失败在无_flag_env_时上报() {
        let err = resolve_endpoint(None, None, Err("bad toml".to_owned())).unwrap_err();
        assert!(matches!(err, CliError::Config(ref msg) if msg.contains("bad toml")));
    }

    #[test]
    fn 读取配置文件_文件不存在返回_none() {
        let dir = tempfile::TempDir::new().unwrap();
        assert_eq!(
            read_config_endpoint(&dir.path().join("none.toml")).unwrap(),
            None
        );
    }

    #[test]
    fn 读取配置文件_解析_endpoint() {
        let (_dir, path) = temp_config("endpoint = \"http://127.0.0.1:8000\"\n");
        assert_eq!(
            read_config_endpoint(&path).unwrap(),
            Some("http://127.0.0.1:8000".to_owned())
        );
    }

    #[test]
    fn 读取配置文件_无_endpoint_键返回_none() {
        let (_dir, path) = temp_config("other = 1\n");
        assert_eq!(read_config_endpoint(&path).unwrap(), None);
    }

    #[test]
    fn 读取配置文件_非法_toml_报错() {
        let (_dir, path) = temp_config("endpoint = [broken\n");
        assert!(read_config_endpoint(&path).unwrap_err().contains("TOML"));
    }

    #[test]
    fn 读取配置文件_endpoint_非字符串报错() {
        let (_dir, path) = temp_config("endpoint = 123\n");
        assert!(
            read_config_endpoint(&path)
                .unwrap_err()
                .contains("必须是字符串")
        );
    }
}
