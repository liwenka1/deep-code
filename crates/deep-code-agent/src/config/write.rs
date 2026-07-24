//! Targeted writes to the global config file (`~/.deep-code/config.toml`).
//!
//! Edits are format-preserving (`toml_edit`): user comments and unrelated
//! fields survive an update. Writes are atomic (tmp + rename) and the file
//! is chmod 600 afterwards — it may hold the API key.

use std::fs;
use std::path::{Path, PathBuf};

use toml_edit::{Document, value};

use crate::i18n::{Lang, TextId, tr, tr_with};

/// One field update to the global config file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GlobalConfigUpdate {
    /// `Some(key)` sets the API key, `None` removes it (logout).
    ApiKey(Option<String>),
    Model(String),
    /// UI language (`ui.language`): "auto" | "zh" | "en".
    Language(String),
}

/// Loose sanity check before persisting a key. Error messages never echo
/// the input back. `lang` localizes the rejection message.
pub fn validate_api_key(text: &str, lang: Lang) -> Result<(), String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Err(tr(lang, TextId::CfgApiKeyEmpty).to_string());
    }
    if trimmed.chars().any(char::is_whitespace) {
        return Err(tr(lang, TextId::CfgApiKeyWhitespace).to_string());
    }
    if trimmed.len() < 16 {
        return Err(tr(lang, TextId::CfgApiKeyTooShort).to_string());
    }
    Ok(())
}

/// Apply one update to the config file at `path`, creating it (with a
/// commented template) when missing. Returns the path written. `lang`
/// localizes any I/O error and the generated template comment.
pub fn write_global_config_update(
    path: &Path,
    update: &GlobalConfigUpdate,
    lang: Lang,
) -> Result<PathBuf, String> {
    let mut document = match fs::read_to_string(path) {
        Ok(existing) => existing.parse::<Document>().map_err(|error| {
            tr_with(
                lang,
                TextId::CfgParseFailed,
                &[
                    ("path", &path.display().to_string()),
                    ("error", &error.to_string()),
                ],
            )
        })?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => new_config_template(lang)
            .parse::<Document>()
            .expect("builtin template parses"),
        Err(error) => {
            return Err(tr_with(
                lang,
                TextId::CfgReadFailed,
                &[
                    ("path", &path.display().to_string()),
                    ("error", &error.to_string()),
                ],
            ));
        }
    };

    let section = match update {
        GlobalConfigUpdate::Language(_) => "ui",
        _ => "provider",
    };
    if document.get(section).is_none() {
        document[section] = toml_edit::table();
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
        GlobalConfigUpdate::Language(language) => {
            document["ui"]["language"] = value(language.as_str());
        }
    }

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            tr_with(
                lang,
                TextId::CfgDirCreateFailed,
                &[
                    ("path", &parent.display().to_string()),
                    ("error", &error.to_string()),
                ],
            )
        })?;
    }
    let tmp = path.with_extension("toml.tmp");
    write_private(&tmp, &document.to_string()).map_err(|error| {
        tr_with(
            lang,
            TextId::CfgWriteFailed,
            &[
                ("path", &tmp.display().to_string()),
                ("error", &error.to_string()),
            ],
        )
    })?;
    fs::rename(&tmp, path).map_err(|error| {
        tr_with(
            lang,
            TextId::CfgReplaceFailed,
            &[
                ("path", &path.display().to_string()),
                ("error", &error.to_string()),
            ],
        )
    })?;
    restrict_permissions(path);
    Ok(path.to_path_buf())
}

fn new_config_template(lang: Lang) -> String {
    format!("{}\n\n[provider]\n", tr(lang, TextId::CfgTemplateHeader))
}

/// Write secret-bearing content to a fresh file created with private
/// permissions from the start, so the API key is never briefly world-readable
/// between creation and the post-rename chmod.
#[cfg(unix)]
fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    // `mode(0o600)` only applies to a freshly created file. A stale tmp left by
    // a prior crash (possibly 0644) would be reused and keep the key briefly
    // world-readable — the exact window this helper exists to close — so drop it
    // first and let `create_new` guarantee a brand-new 0600 file.
    if path.exists() {
        fs::remove_file(path)?;
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(contents.as_bytes())
}

#[cfg(not(unix))]
fn write_private(path: &Path, contents: &str) -> std::io::Result<()> {
    fs::write(path, contents)
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
        assert!(validate_api_key("", Lang::Zh).is_err());
        assert!(validate_api_key("sk short", Lang::Zh).is_err());
        assert!(validate_api_key("short", Lang::Zh).is_err());
        let error = validate_api_key("leaky secret", Lang::En).unwrap_err();
        assert!(!error.contains("leaky"), "errors must not echo the input");
        // Rejections localize with the UI language.
        assert!(error.contains("whitespace"), "{error}");
        assert!(
            validate_api_key("bad key", Lang::Zh)
                .unwrap_err()
                .contains("空白")
        );
        assert!(validate_api_key("sk-0123456789abcdef", Lang::Zh).is_ok());
    }

    #[test]
    fn creates_template_with_key_and_restricts_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let written = write_global_config_update(
            &path,
            &GlobalConfigUpdate::ApiKey(Some("sk-0123456789abcdef".to_string())),
            Lang::Zh,
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
        assert_eq!(
            loaded.config.api_key.as_deref(),
            Some("sk-0123456789abcdef")
        );
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
            Lang::Zh,
        )
        .unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("# 用户自己的注释"), "comments survive");
        assert!(contents.contains("api_key = \"sk-oldoldoldoldold\""));
        assert!(contents.contains("model = \"deepseek-v4-flash\""));
        assert!(contents.contains("currency = \"usd\""));
    }

    #[test]
    fn language_update_writes_ui_section() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[provider]\nmodel = \"auto\"\n").unwrap();

        write_global_config_update(
            &path,
            &GlobalConfigUpdate::Language("en".to_string()),
            Lang::Zh,
        )
        .unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("[ui]"));
        assert!(contents.contains("language = \"en\""));
        assert!(contents.contains("model = \"auto\""), "provider untouched");

        // The layered loader reads it back.
        let loaded = crate::config::AgentConfig::load_with(Some(path), None, &|_| None);
        assert_eq!(loaded.config.language, "en");
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

        write_global_config_update(&path, &GlobalConfigUpdate::ApiKey(None), Lang::Zh).unwrap();

        let contents = fs::read_to_string(&path).unwrap();
        assert!(!contents.contains("api_key"));
        assert!(contents.contains("model = \"auto\""));
    }
}
