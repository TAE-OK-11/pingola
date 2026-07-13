#[cfg(not(feature = "jemalloc"))]
use anyhow::bail;
#[cfg(feature = "jemalloc")]
use anyhow::Context;
use anyhow::Result;
#[cfg(feature = "jemalloc")]
use tikv_jemalloc_ctl::{epoch, stats, version as jemalloc_version};

#[cfg(all(feature = "jemalloc", feature = "system-allocator"))]
compile_error!("jemalloc and system-allocator features are mutually exclusive");

#[cfg(feature = "jemalloc")]
#[global_allocator]
static GLOBAL_ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(feature = "jemalloc")]
pub fn summary(include_stats: bool) -> Result<String> {
    let version = jemalloc_version::read()
        .context("failed to query jemalloc version")?
        .trim_end_matches('\0');
    if !include_stats {
        return Ok(format!("allocator=jemalloc version={version}"));
    }

    epoch::advance().context("failed to refresh jemalloc statistics")?;
    let allocated = stats::allocated::read().context("failed to read stats.allocated")?;
    let active = stats::active::read().context("failed to read stats.active")?;
    let resident = stats::resident::read().context("failed to read stats.resident")?;
    let mapped = stats::mapped::read().context("failed to read stats.mapped")?;
    let retained = stats::retained::read().context("failed to read stats.retained")?;
    let fragmentation = if active == 0 {
        0.0
    } else {
        (resident.saturating_sub(active)) as f64 / active as f64
    };
    Ok(format!(
        "allocator=jemalloc version={version} allocated={allocated} active={active} resident={resident} mapped={mapped} retained={retained} fragmentation_ratio={fragmentation:.4}"
    ))
}

#[cfg(not(feature = "jemalloc"))]
pub fn summary(_include_stats: bool) -> Result<String> {
    Ok("allocator=system".to_owned())
}

pub fn environment_requests_stats() -> bool {
    std::env::var("PINGORA_JEMALLOC_STATS")
        .is_ok_and(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

#[cfg(feature = "jemalloc")]
pub fn detailed_stats() -> Result<serde_json::Value> {
    epoch::advance().context("failed to refresh jemalloc statistics")?;
    let mut options = tikv_jemalloc_ctl::stats_print::Options::default();
    options.json_format = true;
    options.skip_constants = true;
    options.skip_per_arena = true;
    options.skip_bin_size_classes = true;
    options.skip_large_size_classes = true;
    options.skip_mutex_statistics = true;
    let mut output = Vec::with_capacity(16 * 1024);
    tikv_jemalloc_ctl::stats_print::stats_print(&mut output, options)
        .context("failed to print jemalloc statistics")?;
    serde_json::from_slice(&output).context("jemalloc returned invalid statistics JSON")
}

#[cfg(not(feature = "jemalloc"))]
pub fn detailed_stats() -> Result<serde_json::Value> {
    bail!("jemalloc statistics are unavailable with the system allocator build")
}

#[cfg(all(test, feature = "jemalloc"))]
mod tests {
    use super::*;

    #[test]
    fn process_uses_queryable_jemalloc() {
        let result = summary(false).unwrap();
        assert!(result.starts_with("allocator=jemalloc version="));
    }

    #[test]
    fn detailed_statistics_include_allocator_counters() {
        let result = detailed_stats().unwrap();
        assert!(result.get("jemalloc").is_some());
    }
}
