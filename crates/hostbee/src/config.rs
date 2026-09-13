//! 配置文件 `~/.hostbee/config.toml`：endpoint 与全部明文凭据（ADR-0001）的读写。
//!
//! 字段（全部可缺省，空字符串一律归一化为 `None`）：
//!
//! ```toml
//! endpoint      = "http://127.0.0.1:8000"
//! contact       = "admin@example.com"
//! password      = "明文密码"
//! totp_secret   = "TOTP 密钥 Base32"
//! access_token  = "HB-AUTH 用的 access token"
//! refresh_token = "refresh 轮换凭证"
//! ```
//!
//! 写入是**原子**的（临时文件 + 同目录 rename）：refresh 轮换后写盘不会出现
//! 半更新状态——要么旧文件完好，要么新内容完整落盘。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 配置文件所在目录（用户 home 下）。
pub const CONFIG_DIR: &str = ".hostbee";
/// 配置文件名。
pub const CONFIG_FILE: &str = "config.toml";

/// 当前用户的配置文件路径 `~/.hostbee/config.toml`；拿不到 home 目录时返回 None。
pub fn default_config_path() -> Option<PathBuf> {
    std::env::home_dir().map(|home| home.join(CONFIG_DIR).join(CONFIG_FILE))
}

/// 配置文件内容模型（serde 派生保证 TOML 转义正确，密码等特殊字符安全落盘）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Config {
    pub endpoint: Option<String>,
    pub contact: Option<String>,
    pub password: Option<String>,
    pub totp_secret: Option<String>,
    pub access_token: Option<String>,
    pub refresh_token: Option<String>,
}

impl Config {
    /// 序列化为 TOML 文本（`None` 字段不落盘）。
    pub fn to_toml(&self) -> Result<String, String> {
        toml::to_string(self).map_err(|e| format!("配置序列化为 TOML 失败: {e}"))
    }
}

/// 从配置文件读取。文件不存在视为无配置（Ok(None)）；
/// 文件存在但读不了/解析不了是本地错误（Err）。
/// 空字符串字段归一化为 `None`。
pub fn read_config(path: &Path) -> Result<Option<Config>, String> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("读取配置文件 {} 失败: {e}", path.display())),
    };
    let config: Config = toml::from_str(&content)
        .map_err(|e| format!("配置文件 {} 不是合法 TOML: {e}", path.display()))?;
    Ok(Some(Config {
        endpoint: non_empty(config.endpoint),
        contact: non_empty(config.contact),
        password: non_empty(config.password),
        totp_secret: non_empty(config.totp_secret),
        access_token: non_empty(config.access_token),
        refresh_token: non_empty(config.refresh_token),
    }))
}

/// 原子写入：临时文件写同目录 → fsync → rename 替换。
/// 任一时刻磁盘上都只有完整的一份配置：旧文件或新文件，不会半新半旧。
pub fn write_config_atomic(path: &Path, config: &Config) -> Result<(), String> {
    let Some(parent) = path.parent() else {
        return Err(format!("配置文件路径 {} 无父目录", path.display()));
    };
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("创建配置目录 {} 失败: {e}", parent.display()))?;
    let content = config.to_toml()?;
    // 临时文件必须与目标同目录：跨目录 rename 不是原子替换。
    let tmp = parent.join(format!(".{CONFIG_FILE}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, content)
        .map_err(|e| format!("写入临时文件 {} 失败: {e}", tmp.display()))?;
    std::fs::File::open(&tmp)
        .and_then(|f| f.sync_all())
        .map_err(|e| format!("fsync 临时文件 {} 失败: {e}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .map_err(|e| format!("rename {} -> {} 失败: {e}", tmp.display(), path.display()))
}

/// 空字符串视为未提供（与 endpoint 解析口径一致）。
pub fn non_empty(value: Option<String>) -> Option<String> {
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
    fn 读取配置文件_文件不存在返回_none() {
        let dir = tempfile::TempDir::new().unwrap();
        assert_eq!(read_config(&dir.path().join("none.toml")).unwrap(), None);
    }

    #[test]
    fn 读取配置文件_解析全部字段() {
        let (_dir, path) = temp_config(
            r#"
endpoint = "http://127.0.0.1:8000"
contact = "admin@example.com"
password = "p@ss\"wo\\rd"
totp_secret = "MZXW6"
access_token = "acc"
refresh_token = "ref"
"#,
        );
        let config = read_config(&path).unwrap().unwrap();
        assert_eq!(config.endpoint.as_deref(), Some("http://127.0.0.1:8000"));
        assert_eq!(config.contact.as_deref(), Some("admin@example.com"));
        // 密码含引号/反斜杠时 TOML 转义必须无损还原
        assert_eq!(config.password.as_deref(), Some("p@ss\"wo\\rd"));
        assert_eq!(config.totp_secret.as_deref(), Some("MZXW6"));
        assert_eq!(config.access_token.as_deref(), Some("acc"));
        assert_eq!(config.refresh_token.as_deref(), Some("ref"));
    }

    #[test]
    fn 读取配置文件_缺失字段为_none() {
        let (_dir, path) = temp_config("endpoint = \"http://127.0.0.1:8000\"\n");
        let config = read_config(&path).unwrap().unwrap();
        assert_eq!(config.endpoint.as_deref(), Some("http://127.0.0.1:8000"));
        assert_eq!(config.contact, None);
        assert_eq!(config.password, None);
        assert_eq!(config.access_token, None);
        assert_eq!(config.refresh_token, None);
    }

    #[test]
    fn 读取配置文件_空字符串归一化为_none() {
        let (_dir, path) = temp_config("endpoint = \"http://127.0.0.1:8000\"\npassword = \"\"\n");
        let config = read_config(&path).unwrap().unwrap();
        assert_eq!(config.endpoint.as_deref(), Some("http://127.0.0.1:8000"));
        assert_eq!(config.password, None);
    }

    #[test]
    fn 读取配置文件_非法_toml_报错() {
        let (_dir, path) = temp_config("endpoint = [broken\n");
        assert!(read_config(&path).unwrap_err().contains("TOML"));
    }

    #[test]
    fn 读取配置文件_字段非字符串报错() {
        let (_dir, path) = temp_config("endpoint = 123\n");
        let err = read_config(&path).unwrap_err();
        assert!(err.contains("config.toml"), "错误应指向文件: {err}");
        assert!(err.contains("123"), "错误应包含原始值: {err}");
    }

    #[test]
    fn 原子写入_目录不存在时自动创建并落盘() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(".hostbee").join(CONFIG_FILE);
        let config = Config {
            endpoint: Some("http://127.0.0.1:8000".to_owned()),
            password: Some("秘密\"密码".to_owned()),
            ..Config::default()
        };
        write_config_atomic(&path, &config).unwrap();
        let round = read_config(&path).unwrap().unwrap();
        assert_eq!(round, config);
        // toml 文本里没有凭据字段缺失问题（quote 转义正确性由 read 侧断言覆盖）
    }

    #[test]
    fn 原子写入_覆盖已有文件_无临时文件残留() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        write_config_atomic(&path, &Config::default()).unwrap();
        let config = Config {
            endpoint: Some("http://new".to_owned()),
            access_token: Some("acc-2".to_owned()),
            ..Config::default()
        };
        write_config_atomic(&path, &config).unwrap();
        assert_eq!(read_config(&path).unwrap().unwrap(), config);
        // 同目录内不留任何 .tmp 残留（半更新状态不可见）
        let residue: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("tmp"))
            .collect();
        assert_eq!(residue, Vec::<String>::new(), "不应有临时文件残留");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn 原子写入_缺省字段不落盘() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(CONFIG_FILE);
        let config = Config {
            endpoint: Some("http://127.0.0.1:8000".to_owned()),
            ..Config::default()
        };
        write_config_atomic(&path, &config).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("endpoint"));
        assert!(!text.contains("password"), "未设置字段不应落盘: {text}");
        assert!(!text.contains("refresh_token"));
    }
}
