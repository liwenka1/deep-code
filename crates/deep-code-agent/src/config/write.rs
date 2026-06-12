//! Targeted writes to the global config file (`~/.deep-code/config.toml`).
//!
//! Edits are format-preserving (`toml_edit`): user comments and unrelated
//! fields survive an update. Writes are atomic (tmp + rename) and the file
//! is chmod 600 afterwards — it may hold the API key.

use std::fs;
use std::path::{Path, PathBuf};

use toml_edit::{Document, value};

/// One field update for the `[provider]` section of the global config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GlobalConfigUpdate {
    /// `Some(key)` sets the API key, `None` removes it (logout).
    ApiKey(Option<String>),
    Model(String),
}

/// Loose sanity check before persisting a key. Error messages never echo
/// the input back.
pub fn validate_api_key(text: &str) -> Result<(), String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err("API key 为空：用法 /apikey sk-xxxx".to_string());
    }
    if trimmed.chars().any(char::is_whitespace) {
        return Err("API key 不应包含空白字符，请检查复制内容".to_string());
    }
    if trimmed.len() < 16 {
        return Err("API key 长度异常（过短），请检查复制是否完整".to_string());
    }
    Ok(())
}

/// Apply one update to the config file at `path`, creating it (with a
/// commented template) when missing. Returns the path written.
pub fn write_global_config_update(
    path: &Path,
    update: &GlobalConfigUpdate,
) -> Result<PathBuf, String> {
    let mut document = match fs::read_to_string(path) {
        Ok(existing) => existing
            .parse::<Document>()
            .map_err(|error| format!("现有配置 {} 解析失败：{error}", path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => new_config_template()
            .parse::<Document>()
            .expect("builtin template parses"),
        Err(error) => return Err(format!("读取 {} 失败：{error}", path.display())),
    };

    if document.get("provider").is_none() {
        document["provider"] = toml_edit::table();
    }
    match update {
        GlobalConfigUpdate::ApiKey(Some(key)) => {
            document["provider"]["api_key"] = value(key.trim());
        }
        GlobalConfigUpdate::ApiKey(None) => {
            if let Some(provider) = document["provider"].as_table_mut() {
                provider.remove("api_key");
            }
        }
        GlobalConfigUpdate::Model(model) => {
            document["provider"]["model"] = value(model.as_str());
        }
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("创建目录 {} 失败：{error}", parent.display()))?;
    }
    let tmp = path.with_extension("toml.tmp");
    fs::write(&tmp, document.to_string())
        .map_err(|error| format!("写入 {} 失败：{error}", tmp.display()))?;
    fs::rename(&tmp, path)
        .map_err(|error| format!("替换 {} 失败：{error}", path.display()))?;
    restrict_permissions(path);
    Ok(path.to_path_buf())
}

fn new_config_template() -> &'static str {
    r#"# deep-code 全局配置（由 /apikey 或 /model 命令生成）
# 加载顺序：内置默认 -> 本文件 -> 项目 .deep-code/config.toml -> 环境变量
# 完整示例见仓库根目录 config.example.toml

[provider]
"#
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_rejects_bad_keys_without_echoing() {
        assert!(validate_api_key("").is_err());
        assert!(validate_api_key("sk short").is_err());
        assert!(validate_api_key("short").is_err());
        let error = validate_api_key("leaky secret").unwrap_err();
        assert!(!error.contains("leaky"), "errors must not echo the input");
        assert!(validate_api_key("sk-0123456789abcdef").is_ok());
    }

    #[test]
    fn creates_template_with_key_and_restricts_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let written = write_global_config_update(
            &path,
            &GlobalConfigUpdate::ApiKey(Some("sk-0123456789abcdef".to_string())),
        )
        .unwrap();
        assert_eq!(written, path);

        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("api_key = \"sk-0123456789abcdef\""));
        assert!(contents.contains("[provider]"));
        assert!(contents.contains("# deep-code 全局配置"));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        // The layered loader can read it back.
        let loaded = crate::config::AgentConfig::load_with(Some(path), None, &|_| None);
        assert_eq!(loaded.config.api_key.as_deref(), Some("sk-0123456789abcdef"));
    }

    #[test]
    fn update_preserves_comments_and_other_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "# 用户自己的注释\n[provider]\napi_key = \"sk-oldoldoldoldold\"\nmodel = \"auto\" # 行尾注释\n\n[cost]\ncurrency = \"usd\"\n",
        )
        .unwrap();

        write_global_config_update(
            &path,
            &GlobalConfigUpdate::Model("deepseek-v4-flash".to_string()),
        )
        .unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("# 用户自己的注释"), "comments survive");
        assert!(contents.contains("api_key = \"sk-oldoldoldoldold\""));
        assert!(contents.contains("model = \"deepseek-v4-flash\""));
        assert!(contents.contains("currency = \"usd\""));
    }

    #[test]
    fn logout_removes_only_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "[provider]\napi_key = \"sk-deletemedeleteme\"\nmodel = \"auto\"\n",
        )
        .unwrap();

        write_global_config_update(&path, &GlobalConfigUpdate::ApiKey(None)).unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        assert!(!contents.contains("api_key"));
        assert!(contents.contains("model = \"auto\""));
    }
}
