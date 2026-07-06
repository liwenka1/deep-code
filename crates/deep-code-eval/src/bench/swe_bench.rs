//! SWE-bench dataset loader.
//!
//! Loads instances from the HuggingFace datasets-server REST API.
//! The API returns simple JSON (not Parquet), one page at a time.

use serde::Deserialize;

use crate::bench::{BenchmarkSet, SweBenchInstance};

/// HuggingFace datasets-server API base URL for SWE-bench Lite.
const API_BASE: &str =
    "https://datasets-server.huggingface.co/rows";
const DATASET: &str = "princeton-nlp/SWE-bench_Lite";
const CONFIG: &str = "default";
const SPLIT: &str = "test";
const ROWS_PER_PAGE: usize = 100;

/// Response from the datasets-server rows endpoint.
#[derive(Debug, Deserialize)]
struct RowsResponse {
    rows: Vec<RowEntry>,
    #[allow(dead_code)]
    num_rows_total: u64,
    #[allow(dead_code)]
    partial: bool,
}

#[derive(Debug, Deserialize)]
struct RowEntry {
    #[allow(dead_code)]
    row_idx: u64,
    row: SweBenchInstance,
}

/// Load SWE-bench instances from the HuggingFace datasets-server API.
pub async fn load(subset: &str, sample: Option<usize>) -> anyhow::Result<BenchmarkSet<SweBenchInstance>> {
    let mut all_instances = Vec::new();
    let mut offset = 0u64;

    loop {
        let url = format!(
            "{API_BASE}?dataset={DATASET}&config={CONFIG}&split={SPLIT}&offset={offset}&length={ROWS_PER_PAGE}"
        );

        let resp: RowsResponse = reqwest::get(&url)
            .await?
            .json()
            .await?;

        let page_instances: Vec<_> = resp.rows.into_iter().map(|entry| entry.row).collect();
        let page_len = page_instances.len();
        all_instances.extend(page_instances);

        // Stop if we have enough or reached the last page
        if let Some(n) = sample {
            if all_instances.len() >= n {
                all_instances.truncate(n);
                break;
            }
        }

        if page_len < ROWS_PER_PAGE {
            break;
        }

        offset += ROWS_PER_PAGE as u64;
    }

    // Sort by instance_id for deterministic ordering.
    all_instances.sort_by(|a, b| a.instance_id.cmp(&b.instance_id));

    let total = all_instances.len();
    Ok(BenchmarkSet {
        name: format!("swe-bench/{subset}"),
        description: format!("SWE-bench {subset} ({total} instances)"),
        instances: all_instances,
    })
}
