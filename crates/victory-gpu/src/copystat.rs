//! Diagnostics: where the submission copies of title memory go (which
//! ranges, how often, of what memory) and what limits their rate, logged
//! with the frame-rate line. `VICTORY_COPY_MODE=plain` copies without the
//! mapped-page guard, `none` skips the copies (both for timing only).
//! Requested sizes describe logical ranges. Filled shadow sizes include
//! zero fills for unreadable bytes and describe host memory traffic.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Counters since the last line.
pub(crate) enum C {
    Submissions,
    Copies,
    CopyBytes,
    Refreshes,
    RefreshBytes,
    Skipped,
    SkippedBytes,
    Runs,
    RunUs,
    Queries,
    QueryUs,
    MoveUs,
    Unmapped,
    /// Write-watch askings and their time.
    Watches,
    WatchUs,
    /// Bytes of watched ranges checked, and of those found unchanged.
    Checked,
    Unchanged,
    /// Copies that met an unmapped page.
    Fallbacks,
    /// Writes taken by the guard on view memory.
    GuardFaults,
    /// Texture bytes detiled after a watched write; hashed; detiled after a
    /// hash change.
    TexWatched,
    TexHashed,
    TexChanged,
    DirectSlots,
    DirectRanges,
    DirectRequested,
    DirectBytes,
    DirectUs,
    ConventionalRanges,
    ConventionalRequested,
    ConventionalBytes,
    ConventionalUs,
    ShadowAllocations,
    ShadowAllocationBytes,
    ShadowAllocationUs,
    WatchPages,
    WatchDirtyPages,
    WatchInitialPages,
    WatchBytes,
    UnwatchedRanges,
    UnwatchedBytes,
    WatchFailedRanges,
    WatchFailedBytes,
    SnapshotBytes,
    SnapshotUs,
    CopyBackCompared,
    CopyBackBytes,
    CopyBackUs,
    SrvMaterializations,
    SrvMaterializationUs,
}
const COUNTERS: usize = C::SrvMaterializationUs as usize + 1;
static COUNT: [AtomicU64; COUNTERS] = [const { AtomicU64::new(0) }; COUNTERS];

pub(crate) fn add(c: C, n: u64) {
    COUNT[c as usize].fetch_add(n, Relaxed);
}

#[cfg(test)]
pub(crate) fn read(c: C) -> u64 {
    COUNT[c as usize].load(Relaxed)
}

/// How copies run: 0 guarded (as always), 1 plain, 2 not at all.
pub(crate) fn mode() -> u8 {
    static MODE: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("VICTORY_COPY_MODE").as_deref() {
        Ok("plain") => 1,
        Ok("none") => 2,
        _ => 0,
    })
}

#[derive(Default)]
struct RangeStats {
    calls: u64,
    filled: u64,
    us: u64,
}

#[derive(Default)]
struct RefreshStats {
    /// Conventional refreshes by native shadow, title address, and requested size.
    ranges: HashMap<(usize, usize, usize), RangeStats>,
    /// Failed watch probes by native shadow.
    failed: HashMap<usize, (u64, u64)>,
}

static REFRESH: Mutex<Option<RefreshStats>> = Mutex::new(None);

pub(crate) fn range(shadow: usize, src: usize, len: usize, filled: usize, us: u64) {
    let mut stats = REFRESH.lock().unwrap_or_else(|e| e.into_inner());
    let e = stats
        .get_or_insert_with(RefreshStats::default)
        .ranges
        .entry((shadow, src, len))
        .or_default();
    e.calls += 1;
    e.filled += filled as u64;
    e.us += us;
}

pub(crate) fn watch_failed(shadow: usize, requested: usize) {
    add(C::WatchFailedRanges, 1);
    add(C::WatchFailedBytes, requested as u64);
    let mut stats = REFRESH.lock().unwrap_or_else(|e| e.into_inner());
    let (calls, bytes) = stats
        .get_or_insert_with(RefreshStats::default)
        .failed
        .entry(shadow)
        .or_default();
    *calls += 1;
    *bytes += requested as u64;
}

fn page_faults() -> u64 {
    use windows_sys::Win32::System::ProcessStatus::{
        K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut c = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: our own process and a sized structure.
    unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
    u64::from(c.PageFaultCount)
}

/// Logs the counters per frame over `frames` frames, and the ranges that
/// took most bytes.
pub(crate) fn log(frames: f64) {
    static FAULTS: AtomicU64 = AtomicU64::new(0);
    let faults = page_faults();
    let faults = faults - FAULTS.swap(faults, Relaxed).min(faults);
    let c: Vec<f64> = COUNT
        .iter()
        .map(|a| a.swap(0, Relaxed) as f64 / frames)
        .collect();
    let mb = |v: f64| v / 1e6;
    let ms = |v: f64| v / 1e3;
    tracing::info!(
        "copies per frame: {:.1} submissions; {:.0} title copies {:.1} MB; {:.0} refreshes {:.1} MB, \
         {:.0} skipped {:.1} MB; {:.0} runs {:.1} ms; {:.0} queries {:.2} ms, moves {:.2} ms, \
         {:.2} MB unmapped; {:.0} page faults; {:.0} watches {:.2} ms, {:.1} MB checked, {:.1} MB \
         unchanged; {:.1} fallbacks; {:.0} guard faults; textures {:.1} MB watched and \
         changed, {:.1} MB hashed, {:.1} MB of them changed",
        c[C::Submissions as usize],
        c[C::Copies as usize],
        mb(c[C::CopyBytes as usize]),
        c[C::Refreshes as usize],
        mb(c[C::RefreshBytes as usize]),
        c[C::Skipped as usize],
        mb(c[C::SkippedBytes as usize]),
        c[C::Runs as usize],
        ms(c[C::RunUs as usize]),
        c[C::Queries as usize],
        ms(c[C::QueryUs as usize]),
        ms(c[C::MoveUs as usize]),
        mb(c[C::Unmapped as usize]),
        faults as f64 / frames,
        c[C::Watches as usize],
        ms(c[C::WatchUs as usize]),
        mb(c[C::Checked as usize]),
        mb(c[C::Unchanged as usize]),
        c[C::Fallbacks as usize],
        c[C::GuardFaults as usize],
        mb(c[C::TexWatched as usize]),
        mb(c[C::TexHashed as usize]),
        mb(c[C::TexChanged as usize]),
    );
    if c[C::DirectSlots as usize] > 0.0 {
        tracing::info!(
            "direct SRVs per frame: {:.0} slots, {:.0} merged ranges, {:.2} MB requested, {:.2} MB filled, {:.2} ms preparing",
            c[C::DirectSlots as usize],
            c[C::DirectRanges as usize],
            mb(c[C::DirectRequested as usize]),
            mb(c[C::DirectBytes as usize]),
            ms(c[C::DirectUs as usize]),
        );
    }
    let value = |counter: C| c[counter as usize];
    tracing::info!(
        frames,
        conventional_ranges = value(C::ConventionalRanges),
        conventional_requested_mb = mb(value(C::ConventionalRequested)),
        conventional_filled_mb = mb(value(C::ConventionalBytes)),
        conventional_ms = ms(value(C::ConventionalUs)),
        watched_pages = value(C::WatchPages),
        dirty_pages = value(C::WatchDirtyPages),
        initial_pages = value(C::WatchInitialPages),
        watched_filled_mb = mb(value(C::WatchBytes)),
        untracked_ranges = value(C::UnwatchedRanges),
        untracked_filled_mb = mb(value(C::UnwatchedBytes)),
        watch_failures = value(C::WatchFailedRanges),
        watch_failed_mb = mb(value(C::WatchFailedBytes)),
        shadow_allocations = value(C::ShadowAllocations),
        shadow_allocated_mb = mb(value(C::ShadowAllocationBytes)),
        shadow_allocate_ms = ms(value(C::ShadowAllocationUs)),
        snapshot_mb = mb(value(C::SnapshotBytes)),
        snapshot_ms = ms(value(C::SnapshotUs)),
        copyback_compared_mb = mb(value(C::CopyBackCompared)),
        copyback_filled_mb = mb(value(C::CopyBackBytes)),
        copyback_ms = ms(value(C::CopyBackUs)),
        srv_materializations = value(C::SrvMaterializations),
        srv_materialization_ms = ms(value(C::SrvMaterializationUs)),
        "shadow traffic per frame"
    );
    let RefreshStats { ranges, failed } = REFRESH
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .unwrap_or_default();
    let mut ranges: Vec<_> = ranges.into_iter().collect();
    ranges.sort_by_key(|(_, stats)| std::cmp::Reverse(stats.filled));
    for ((shadow, src, len), stats) in ranges.into_iter().take(10) {
        use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, VirtualQuery};
        // SAFETY: plain data filled in by the query.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a valid out pointer of the right size.
        unsafe { VirtualQuery(src as *const _, &mut info, size_of_val(&info)) };
        let kind = info.Type;
        let (failed_probes, failed_bytes) = failed.get(&shadow).copied().unwrap_or_default();
        tracing::info!(
            "  copied from {src:#x}+{len:#x} (memory {kind:#x}, shadow {shadow:#x}): {:.1}x/frame, {:.2} MB requested/frame, {:.2} MB filled/frame, {:.2} ms/frame, {:.1} failed watch probes/frame ({:.2} MB requested/frame)",
            stats.calls as f64 / frames,
            (stats.calls * len as u64) as f64 / 1e6 / frames,
            stats.filled as f64 / 1e6 / frames,
            stats.us as f64 / 1e3 / frames,
            failed_probes as f64 / frames,
            failed_bytes as f64 / 1e6 / frames,
        );
    }
}
