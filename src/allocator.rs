#[cfg(feature = "tcmalloc")]
use anyhow::bail;
#[cfg(feature = "jemalloc")]
use anyhow::Context;
use anyhow::Result;
#[cfg(feature = "jemalloc")]
use tikv_jemalloc_ctl::{epoch, stats, version as jemalloc_version};

#[cfg(any(
    all(feature = "jemalloc", feature = "tcmalloc"),
    all(feature = "jemalloc", feature = "system-allocator"),
    all(feature = "tcmalloc", feature = "system-allocator")
))]
compile_error!("select exactly one allocator feature");

#[cfg(not(any(
    feature = "jemalloc",
    feature = "tcmalloc",
    feature = "system-allocator"
)))]
compile_error!("select one allocator feature: tcmalloc, jemalloc, or system-allocator");

#[cfg(feature = "jemalloc")]
#[global_allocator]
static GLOBAL_ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(feature = "tcmalloc")]
#[global_allocator]
static GLOBAL_ALLOCATOR: tcmalloc_better::TCMalloc = tcmalloc_better::TCMalloc;

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

#[cfg(feature = "tcmalloc")]
pub fn summary(include_stats: bool) -> Result<String> {
    let base = format!(
        "allocator=tcmalloc implementation=google-tcmalloc logical_page_size=8192 background_actions_needed={}",
        tcmalloc_better::TCMalloc::needs_process_background_actions()
    );
    if !include_stats {
        return Ok(base);
    }

    let stats = read_tcmalloc_stats()?;
    Ok(format!(
        "{base} allocated={} heap={} physical={} virtual={} peak={} realized_fragmentation_percent={} per_cpu_caches_active={}",
        stats.current_allocated_bytes,
        stats.heap_size,
        stats.physical_memory_used,
        stats.virtual_memory_used,
        stats.peak_memory_usage,
        stats.realized_fragmentation_percent,
        stats.per_cpu_caches_active,
    ))
}

#[cfg(feature = "system-allocator")]
pub fn summary(_include_stats: bool) -> Result<String> {
    Ok("allocator=system".to_owned())
}

pub fn environment_requests_stats() -> bool {
    stats_requested_by("PINGORA_ALLOCATOR_STATS") || stats_requested_by("PINGORA_JEMALLOC_STATS")
}

fn stats_requested_by(name: &str) -> bool {
    std::env::var(name)
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

#[cfg(feature = "tcmalloc")]
pub fn detailed_stats() -> Result<serde_json::Value> {
    let stats = read_tcmalloc_stats()?;
    Ok(serde_json::json!({
        "allocator": "tcmalloc",
        "implementation": "google-tcmalloc",
        "logical_page_size": 8192,
        "background_actions_needed": tcmalloc_better::TCMalloc::needs_process_background_actions(),
        "current_allocated_bytes": stats.current_allocated_bytes,
        "heap_size": stats.heap_size,
        "physical_memory_used": stats.physical_memory_used,
        "virtual_memory_used": stats.virtual_memory_used,
        "peak_memory_usage": stats.peak_memory_usage,
        "realized_fragmentation_percent": stats.realized_fragmentation_percent,
        "per_cpu_caches_active": stats.per_cpu_caches_active,
    }))
}

#[cfg(feature = "tcmalloc")]
#[derive(Debug)]
struct TcmallocStats {
    current_allocated_bytes: usize,
    heap_size: usize,
    physical_memory_used: usize,
    virtual_memory_used: usize,
    peak_memory_usage: usize,
    realized_fragmentation_percent: usize,
    per_cpu_caches_active: bool,
}

#[cfg(feature = "tcmalloc")]
fn read_tcmalloc_stats() -> Result<TcmallocStats> {
    Ok(TcmallocStats {
        current_allocated_bytes: tcmalloc_numeric_property("generic.current_allocated_bytes")?,
        heap_size: tcmalloc_numeric_property("generic.heap_size")?,
        physical_memory_used: tcmalloc_numeric_property("generic.physical_memory_used")?,
        virtual_memory_used: tcmalloc_numeric_property("generic.virtual_memory_used")?,
        peak_memory_usage: tcmalloc_numeric_property("generic.peak_memory_usage")?,
        realized_fragmentation_percent: tcmalloc_numeric_property(
            "generic.realized_fragmentation",
        )?,
        per_cpu_caches_active: tcmalloc_numeric_property("tcmalloc.per_cpu_caches_active")? != 0,
    })
}

#[cfg(feature = "tcmalloc")]
fn tcmalloc_numeric_property(name: &'static str) -> Result<usize> {
    let mut value = 0_usize;
    // This read-only C ABI is defined by the pinned Google TCMalloc source in
    // libtcmalloc-sys. The string pointer remains valid for the entire call and
    // `value` is an initialized, uniquely borrowed output slot.
    let found = unsafe {
        tcmalloc_get_numeric_property(name.as_ptr().cast(), name.len(), &mut value as *mut usize)
    };
    if !found {
        bail!("Google TCMalloc does not expose numeric property {name}");
    }
    Ok(value)
}

#[cfg(feature = "tcmalloc")]
extern "C" {
    #[link_name = "MallocExtension_Internal_GetNumericProperty"]
    fn tcmalloc_get_numeric_property(
        name: *const std::ffi::c_char,
        name_length: usize,
        value: *mut usize,
    ) -> bool;
}

#[cfg(feature = "system-allocator")]
pub fn detailed_stats() -> Result<serde_json::Value> {
    Ok(serde_json::json!({
        "allocator": "system",
        "note": "use benchmark cgroup and smaps samples for comparable process memory metrics"
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_uses_selected_allocator() {
        let result = summary(false).unwrap();
        #[cfg(feature = "jemalloc")]
        assert!(result.starts_with("allocator=jemalloc version="));
        #[cfg(feature = "tcmalloc")]
        assert!(result.starts_with("allocator=tcmalloc implementation=google-tcmalloc"));
        #[cfg(feature = "system-allocator")]
        assert_eq!(result, "allocator=system");
    }

    #[cfg(feature = "jemalloc")]
    #[test]
    fn detailed_statistics_include_jemalloc_counters() {
        let result = detailed_stats().unwrap();
        assert!(result.get("jemalloc").is_some());
    }

    #[cfg(feature = "tcmalloc")]
    #[test]
    fn detailed_statistics_identify_and_query_tcmalloc() {
        let stats = detailed_stats().unwrap();
        assert_eq!(stats["allocator"], "tcmalloc");
        assert!(stats["current_allocated_bytes"].as_u64().unwrap() > 0);
        assert!(stats["heap_size"].as_u64().unwrap() > 0);
    }
}
