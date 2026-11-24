//! Where the host memory goes: counters of what this crate holds for the
//! title, logged with the process's commit charge (see
//! `victory_hle`'s xmem memory log), to tell the title's own use from ours
//! when loading the world nears the host's commit limit.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Bytes of the title's D3D12 heaps and committed resources (title memory
/// we allocate, from its allocator or ours).
pub(crate) static TITLE_GPU: AtomicU64 = AtomicU64::new(0);
/// Bytes of the host copies of title buffers and loose memory (system
/// memory visible to the GPU), and how many there are.
pub(crate) static SHADOWS: AtomicU64 = AtomicU64::new(0);
pub(crate) static SHADOW_COUNT: AtomicU64 = AtomicU64::new(0);

/// Host textures (the driver's allocations, video memory while it fits),
/// how many, and how many are textures the title placed itself, never
/// released (`resource::texture_at`).
pub(crate) static HOST_TEXTURES: AtomicU64 = AtomicU64::new(0);
pub(crate) static HOST_TEXTURE_COUNT: AtomicU64 = AtomicU64::new(0);
pub(crate) static PLACED_COUNT: AtomicU64 = AtomicU64::new(0);

pub(crate) fn add(counter: &AtomicU64, bytes: u64) {
    counter.fetch_add(bytes, Relaxed);
}

pub(crate) fn sub(counter: &AtomicU64, bytes: u64) {
    // Saturating: a counter never wraps if a release is counted twice.
    let _ = counter.fetch_update(Relaxed, Relaxed, |v| Some(v.saturating_sub(bytes)));
}

/// One line of MiB figures: what this crate holds, and what the host GPU
/// driver says it uses in video and system memory.
pub fn report() -> String {
    let mib = |v: u64| v >> 20;
    #[allow(unused_mut)]
    let mut line = format!(
        "title gpu heaps {} MiB, buffer copies {} MiB ({}), host textures {} MiB ({}, {} placed)",
        mib(TITLE_GPU.load(Relaxed)),
        mib(SHADOWS.load(Relaxed)),
        SHADOW_COUNT.load(Relaxed),
        mib(HOST_TEXTURES.load(Relaxed)),
        HOST_TEXTURE_COUNT.load(Relaxed),
        PLACED_COUNT.load(Relaxed),
    );
    #[cfg(windows)]
    {
        use std::fmt::Write;
        let _ = write!(
            line,
            ", texture staging {} MiB, snapshots {} MiB",
            mib(crate::texture::staged_bytes()),
            mib(crate::shadow::pool_bytes()),
        );
        if let Some([local, system]) = crate::host::video_memory() {
            let _ = write!(
                line,
                ", driver: video {} of {} MiB, system {} MiB",
                mib(local.0),
                mib(local.1),
                mib(system.0)
            );
        }
    }
    line
}
