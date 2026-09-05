#[cfg(feature = "jemalloc")]
use anyhow::Context;
use anyhow::Result;
#[cfg(feature = "tcmalloc")]
use anyhow::bail;
#[cfg(feature = "jemalloc")]
use tikv_jemalloc_ctl::{background_thread, epoch, stats, version as jemalloc_version};

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

#[cfg(feature = "tcmalloc")]
const TCMALLOC_MAX_PER_CPU_CACHE_BYTES: i32 = 256 * 1024;
#[cfg(feature = "tcmalloc")]
const TCMALLOC_BACKGROUND_RELEASE_BYTES_PER_SECOND: usize = 8 * 1024 * 1024;

/// jemalloc dirty-page decay for a 1 vCPU / 1 GiB proxy. Matches Docker
/// `MALLOC_CONF`. Applied again at runtime so a missing env var cannot leave
/// the process on jemalloc's multi-second defaults.
#[cfg(feature = "jemalloc")]
const JEMALLOC_DIRTY_DECAY_MS: isize = 1000;
/// Skip the muzzy purge stage. Returning pages once via dirty decay keeps RSS
/// bounded without a second scan on the background thread.
#[cfg(feature = "jemalloc")]
const JEMALLOC_MUZZY_DECAY_MS: isize = 0;

/// Bound allocator-side retention before worker threads begin serving.
///
/// Google TCMalloc defaults to as much as 1.5 MiB of cache per visible CPU.
/// Containers can see far more host CPUs than their CPU quota, so the default
/// can retain disproportionate RSS on a 1 GiB service. A 256 KiB cache keeps
/// the fast per-CPU path while background release steadily returns idle pages.
///
/// jemalloc uses a single arena (`narenas:1` in `MALLOC_CONF`) for the same
/// reason: `percpu_arena` would create one arena per *visible* host CPU.
pub fn configure_for_proxy() {
    #[cfg(feature = "tcmalloc")]
    unsafe {
        tcmalloc_set_max_per_cpu_cache_size(TCMALLOC_MAX_PER_CPU_CACHE_BYTES);
        tcmalloc_set_background_release_rate(TCMALLOC_BACKGROUND_RELEASE_BYTES_PER_SECOND);
    }
    #[cfg(feature = "jemalloc")]
    configure_jemalloc();
}

#[cfg(feature = "jemalloc")]
fn configure_jemalloc() {
    let _ = background_thread::write(true);
    // Boot-time MALLOC_CONF is the source of truth for narenas/tcache. Decay
    // intervals remain writable so a process started without the image env
    // still returns idle pages on a 1 GiB cgroup.
    let _ = unsafe {
        tikv_jemalloc_ctl::raw::write(b"arenas.dirty_decay_ms\0", JEMALLOC_DIRTY_DECAY_MS)
    };
    let _ = unsafe {
        tikv_jemalloc_ctl::raw::write(b"arenas.muzzy_decay_ms\0", JEMALLOC_MUZZY_DECAY_MS)
    };
}

/// Start a lightweight background thread that returns idle TCMalloc pages to the
/// OS. Without this, RSS can remain elevated long after QUIC/H3 load subsides.
///
/// jemalloc does not need a process thread here: `background_thread:true` plus
/// dirty-page decay already purge unused extents.
pub fn start_background_reclaimer() {
    #[cfg(feature = "tcmalloc")]
    if tcmalloc_better::TCMalloc::needs_process_background_actions() {
        std::thread::Builder::new()
            .name("tcmalloc-reclaim".into())
            .spawn(|| {
                loop {
                    tcmalloc_better::TCMalloc::process_background_actions();
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
            })
            .ok();
    }
}

/// Nudge the allocator to release free pages after a QUIC connection closes.
pub fn hint_release_idle_pages() {
    #[cfg(feature = "tcmalloc")]
    if tcmalloc_better::TCMalloc::needs_process_background_actions() {
        tcmalloc_better::TCMalloc::process_background_actions();
    }
    #[cfg(feature = "jemalloc")]
    {
        // Purge unused dirty pages on every arena. `arena.MALLCTL_ARENAS_ALL`
        // (`u32::MAX`) is the jemalloc all-arenas sentinel.
        let _ = unsafe { tikv_jemalloc_ctl::raw::write(b"arena.4294967295.purge\0", ()) };
    }
}

#[cfg(feature = "jemalloc")]
pub fn summary(include_stats: bool) -> Result<String> {
    let version = jemalloc_version::read()
        .context("failed to query jemalloc version")?
        .trim_end_matches('\0');
    let narenas = tikv_jemalloc_ctl::opt::narenas::read().unwrap_or(0);
    let background = background_thread::read().unwrap_or(false);
    if !include_stats {
        return Ok(format!(
            "allocator=jemalloc version={version} narenas={narenas} background_thread={background}"
        ));
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
        "allocator=jemalloc version={version} narenas={narenas} background_thread={background} allocated={allocated} active={active} resident={resident} mapped={mapped} retained={retained} fragmentation_ratio={fragmentation:.4}"
    ))
}

#[cfg(feature = "tcmalloc")]
pub fn summary(include_stats: bool) -> Result<String> {
    let base = format!(
        "allocator=tcmalloc implementation=google-tcmalloc logical_page_size=8192 per_cpu_cache_limit={} background_release_bytes_per_second={} background_actions_needed={}",
        TCMALLOC_MAX_PER_CPU_CACHE_BYTES,
        TCMALLOC_BACKGROUND_RELEASE_BYTES_PER_SECOND,
        tcmalloc_better::TCMalloc::needs_process_background_actions(),
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
        "per_cpu_cache_limit": TCMALLOC_MAX_PER_CPU_CACHE_BYTES,
        "background_release_bytes_per_second": TCMALLOC_BACKGROUND_RELEASE_BYTES_PER_SECOND,
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
unsafe extern "C" {
    #[link_name = "MallocExtension_Internal_GetNumericProperty"]
    fn tcmalloc_get_numeric_property(
        name: *const std::ffi::c_char,
        name_length: usize,
        value: *mut usize,
    ) -> bool;

    #[link_name = "TCMalloc_Internal_SetMaxPerCpuCacheSize"]
    fn tcmalloc_set_max_per_cpu_cache_size(value: i32);

    #[link_name = "TCMalloc_Internal_SetBackgroundReleaseRate"]
    fn tcmalloc_set_background_release_rate(bytes_per_second: usize);
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
        {
            assert!(result.starts_with("allocator=jemalloc version="));
            assert!(result.contains("narenas="));
            assert!(result.contains("background_thread="));
        }
        #[cfg(feature = "tcmalloc")]
        assert!(result.starts_with("allocator=tcmalloc implementation=google-tcmalloc"));
        #[cfg(feature = "system-allocator")]
        assert_eq!(result, "allocator=system");
    }

    #[cfg(feature = "jemalloc")]
    #[test]
    fn jemalloc_background_thread_is_enabled() {
        configure_jemalloc();
        assert!(background_thread::read().unwrap());
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
