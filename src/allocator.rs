use anyhow::{Context, Result};
use tikv_jemalloc_ctl::{epoch, stats, version as jemalloc_version};

#[global_allocator]
static GLOBAL_ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

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

pub fn environment_requests_stats() -> bool {
    std::env::var("PINGORA_JEMALLOC_STATS")
        .is_ok_and(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_uses_queryable_jemalloc() {
        let result = summary(false).unwrap();
        assert!(result.starts_with("allocator=jemalloc version="));
    }
}
