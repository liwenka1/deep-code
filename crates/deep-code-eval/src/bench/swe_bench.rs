//! SWE-bench dataset loader.
//!
//! Loads instances from the HuggingFace datasets-server REST API (plain JSON,
//! paged). Subset/split map to the official datasets:
//!
//! | subset   | dataset                          | splits      |
//! |----------|----------------------------------|-------------|
//! | lite     | princeton-nlp/SWE-bench_Lite     | dev, test   |
//! | verified | princeton-nlp/SWE-bench_Verified | test        |

use serde::Deserialize;

use crate::bench::{BenchmarkSet, SweBenchInstance};

const DEFAULT_API_BASE: &str = "https://datasets-server.huggingface.co";
/// Override the datasets-server base URL (e.g. a self-hosted proxy) when
/// huggingface.co is unreachable. reqwest additionally honours the standard
/// `HTTPS_PROXY`/`HTTP_PROXY` env vars out of the box.
const API_BASE_ENV: &str = "DEEP_CODE_HF_BASE";
const CONFIG: &str = "default";
const ROWS_PER_PAGE: usize = 100;

fn api_base() -> String {
    std::env::var(API_BASE_ENV)
        .ok()
        .map(|value| value.trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| DEFAULT_API_BASE.to_string())
}

fn dataset_for(subset: &str, split: &str) -> anyhow::Result<&'static str> {
    match (subset, split) {
        ("lite", "dev" | "test") => Ok("princeton-nlp/SWE-bench_Lite"),
        ("verified", "test") => Ok("princeton-nlp/SWE-bench_Verified"),
        ("verified", other) => {
            anyhow::bail!("subset 'verified' only has a 'test' split (got '{other}')")
        }
        (other, _) => anyhow::bail!("unknown subset '{other}' (supported: lite, verified)"),
    }
}

/// Response from the datasets-server rows endpoint.
#[derive(Debug, Deserialize)]
struct RowsResponse {
    rows: Vec<RowEntry>,
}

#[derive(Debug, Deserialize)]
struct RowEntry {
    row: SweBenchInstance,
}

/// Load SWE-bench instances from the HuggingFace datasets-server API.
pub async fn load(
    subset: &str,
    split: &str,
    sample: Option<usize>,
) -> anyhow::Result<BenchmarkSet<SweBenchInstance>> {
    let dataset = dataset_for(subset, split)?;
    let base = api_base();
    let mut all_instances = Vec::new();
    let mut offset = 0u64;

    loop {
        let url = format!(
            "{base}/rows?dataset={dataset}&config={CONFIG}&split={split}&offset={offset}&length={ROWS_PER_PAGE}"
        );
        let network_hint = |error: reqwest::Error| {
            anyhow::anyhow!(
                "无法访问 datasets-server({base}): {error}\n\
提示:huggingface.co 在部分网络不可达(连接可能在传输中被重置)。可选:\n\
  1) 走代理:HTTPS_PROXY=http://127.0.0.1:<port> deep-code eval ...\n\
  2) 自定义镜像/代理地址:{API_BASE_ENV}=<base-url>"
            )
        };
        let response = reqwest::get(&url).await.map_err(network_hint)?;
        if !response.status().is_success() {
            anyhow::bail!(
                "datasets-server returned {} for {dataset}/{split} (offset {offset})",
                response.status()
            );
        }
        let page: RowsResponse = response.json().await.map_err(network_hint)?;
        let page_len = page.rows.len();
        all_instances.extend(page.rows.into_iter().map(|entry| entry.row));

        if page_len < ROWS_PER_PAGE {
            break;
        }
        offset += ROWS_PER_PAGE as u64;
    }

    // Deterministic ordering FIRST, then sample — so `--sample N` selects the
    // same N instances regardless of dataset pagination order.
    all_instances.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));
    if let Some(n) = sample {
        all_instances.truncate(n);
    }

    let total = all_instances.len();
    Ok(BenchmarkSet {
        name: format!("swe-bench/{subset}/{split}"),
        description: format!("SWE-bench {subset} {split} ({total} instances)"),
        instances: all_instances,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dataset_mapping_is_honest() {
        assert!(dataset_for("lite", "dev").is_ok());
        assert!(dataset_for("lite", "test").is_ok());
        assert!(dataset_for("verified", "test").is_ok());
        assert!(dataset_for("verified", "dev").is_err());
        assert!(dataset_for("full", "test").is_err());
    }
}
