//! Indirect execution's arguments, bounded before the GPU reads them. The
//! title's own compute passes write most of them; a pass we drop or
//! translate wrongly leaves whatever the memory held, and a draw of four
//! billion vertices or a dispatch past the host's limits runs until the
//! host resets the display driver. So every record is checked on the GPU,
//! just before its execution, by a compute shader of ours (HLSL compiled by
//! the system's compiler; a 5.1 shader with its root signature inside).
//!
//! Each list also has a budget of 2^27 work units (one per vertex or index
//! instance, 64 per compute group, 128 per mesh group). Records that do not
//! fit run empty.
//!
//! Draws and one-dimensional mesh dispatches retain their existing clamps.
//! Compute dispatches above 65,535 groups on an axis or 2^20 groups total
//! run empty. Dimensions within those limits are preserved.

use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;
use windows::core::{Interface, s};

use crate::host::{HostList, host, transition};

const COMMON_ACCESS: (D3D12_BARRIER_SYNC, D3D12_BARRIER_ACCESS) =
    (D3D12_BARRIER_SYNC_ALL, D3D12_BARRIER_ACCESS_COMMON);
const COMPUTE_UAV: (D3D12_BARRIER_SYNC, D3D12_BARRIER_ACCESS) = (
    D3D12_BARRIER_SYNC_COMPUTE_SHADING,
    D3D12_BARRIER_ACCESS_UNORDERED_ACCESS,
);

const SOURCE: &str = r#"
#define RS "RootConstants(num32BitConstants=4, b0), UAV(u0), UAV(u1)"
cbuffer Record : register(b0) { uint kind; uint stride; uint records; uint unused; };
RWByteAddressBuffer args : register(u0);
// Work reserved by earlier records in the list.
RWByteAddressBuffer budget : register(u1);
static const uint BUDGET = 1u << 27;

bool reserve(uint cost) {
    uint before;
    budget.InterlockedAdd(0, 0, before);
    [allow_uav_condition]
    while (true) {
        if (before > BUDGET || cost > BUDGET - before) return false;
        uint observed;
        budget.InterlockedCompareExchange(0, before, before + cost, observed);
        if (observed == before) return true;
        before = observed;
    }
}

[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    if (id.x >= records) return;
    uint at = id.x * stride;
    uint cost;
    if (kind <= 1) {
        // Draws: vertex or index count, then instances.
        uint count = min(args.Load(at), 1u << 24);
        uint instances = min(args.Load(at + 4), (1u << 26) / max(count, 1u));
        cost = count * instances;
        args.Store2(at, uint2(count, instances));
    } else if (kind == 2) {
        uint3 g = args.Load3(at);
        if (any(g > 65535u)) {
            // Even an empty dispatch must have legal dimensions.
            args.Store3(at, uint3(0, 0, 0));
            return;
        }
        if (any(g == 0u)) return;
        // Two legal dimensions fit in a uint. Check the third before
        // multiplying so the cost always covers the dimensions we keep.
        uint xy = g.x * g.y;
        if (xy > (1u << 20) / g.z) {
            args.Store(at, 0);
            return;
        }
        cost = xy * g.z * 64;
    } else {
        // Mesh dispatches: the console's are one-dimensional.
        uint3 g = args.Load3(at);
        g = uint3(min(g.x, 65535u), min(g.y, 1u), min(g.z, 1u));
        cost = g.x * g.y * g.z * 128;
        args.Store3(at, g);
    }
    if (cost != 0) {
        if (!reserve(cost)) {
            // Zero the vertex/index count or the X group count.
            args.Store(at, 0);
        }
    }
}
"#;

struct Clamp {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
}

// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Clamp {}
// SAFETY: as above.
unsafe impl Sync for Clamp {}

fn clamp() -> Option<&'static Clamp> {
    static CLAMP: std::sync::OnceLock<Option<Clamp>> = std::sync::OnceLock::new();
    CLAMP
        .get_or_init(|| match make() {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!(error = %e, "no argument clamp: indirect execution dropped");
                None
            }
        })
        .as_ref()
}

fn make() -> windows::core::Result<Clamp> {
    let device = &host().ok_or_else(windows::core::Error::empty)?.device;
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: the source and names live through the call; out pointers ours.
    let compiled = unsafe {
        D3DCompile(
            SOURCE.as_ptr().cast(),
            SOURCE.len(),
            s!("clamp"),
            None,
            None,
            s!("main"),
            s!("cs_5_1"),
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = compiled {
        if let Some(errors) = errors {
            // SAFETY: the blob's own bytes.
            let text = unsafe {
                std::slice::from_raw_parts(errors.GetBufferPointer().cast::<u8>(), errors.GetBufferSize())
            };
            tracing::warn!("argument clamp: {}", String::from_utf8_lossy(text));
        }
        return Err(e);
    }
    let code = code.ok_or_else(windows::core::Error::empty)?;
    // SAFETY: the blob's own bytes; the root signature is inside the shader.
    unsafe {
        let bytes = std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize());
        let root: ID3D12RootSignature = device.CreateRootSignature(0, bytes)?;
        let pso = device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: std::mem::transmute_copy(&root),
            CS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: bytes.as_ptr().cast(),
                BytecodeLength: bytes.len(),
            },
            ..Default::default()
        })?;
        Ok(Clamp { root, pso })
    }
}

const THREADS_PER_GROUP: u32 = 64;

fn argument_size(kind: u32) -> Option<u32> {
    match kind {
        0 => Some(16),
        1 => Some(20),
        2 | 10 => Some(12),
        _ => None,
    }
}

/// Span through the last argument, subject to the validator's dispatch
/// size and 32-bit byte offsets.
pub(crate) fn byte_len(kind: u32, stride: u32, records: u32) -> Option<u64> {
    let minimum = argument_size(kind)?;
    if stride < minimum
        || !stride.is_multiple_of(4)
        || records.div_ceil(THREADS_PER_GROUP) > D3D12_CS_DISPATCH_MAX_THREAD_GROUPS_PER_DIMENSION
    {
        return None;
    }
    if records == 0 {
        return Some(0);
    }
    let end = u64::from(records - 1) * u64::from(stride) + u64::from(minimum);
    (end <= 1 << 32).then_some(end)
}

fn enhanced_barriers() -> bool {
    #[cfg(test)]
    if std::env::var_os("VICTORY_TEST_LEGACY_INDIRECT").is_some() {
        return false;
    }
    static SUPPORTED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *SUPPORTED.get_or_init(|| {
        let Some(host) = host() else { return false };
        let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS12::default();
        // SAFETY: the feature query writes the matching options structure.
        let supported = unsafe {
            host.device.CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS12,
                (&raw mut options).cast(),
                size_of_val(&options) as u32,
            )
        }
        .is_ok()
            && options.EnhancedBarriersSupported.as_bool();
        if !supported {
            tracing::info!("indirect execution uses legacy buffer barriers");
        }
        supported
    })
}

pub(crate) fn enhanced_list(list: &HostList) -> Option<ID3D12GraphicsCommandList7> {
    enhanced_barriers().then(|| list.list.cast().ok()).flatten()
}

/// Validates the arguments in place and makes the native inputs readable.
/// Replaces the list's compute root signature and pipeline.
pub(crate) fn bound(
    list: &HostList,
    buffer: &ID3D12Resource,
    offset: u64,
    count: Option<&ID3D12Resource>,
    (kind, stride, records): (u32, u32, u32),
) -> bool {
    let Some(len) = byte_len(kind, stride, records) else {
        return false;
    };
    if len == 0 || !offset.is_multiple_of(4) {
        return false;
    }
    // SAFETY: buffer is a live host resource.
    let (width, gpu) = unsafe { (buffer.GetDesc().Width, buffer.GetGPUVirtualAddress()) };
    if offset.checked_add(len).is_none_or(|end| end > width) {
        return false;
    }
    let Some(gpu) = gpu.checked_add(offset) else {
        return false;
    };
    let (Some(c), Some(budget)) = (clamp(), list.budget()) else {
        return false;
    };
    let l = &list.list;
    let enhanced = enhanced_list(list);
    let separate_count = count.filter(|&count| count != buffer);
    let written = [buffer, budget];
    if let Some(enhanced) = &enhanced {
        list.use_indirect_buffers(
            [Some(buffer), separate_count, Some(budget)]
                .into_iter()
                .flatten(),
        );
        buffer_barriers(enhanced, written, COMMON_ACCESS, COMPUTE_UAV);
    } else {
        uav_barrier(l);
    }
    let constants = [kind, stride, records, 0];
    // SAFETY: an open list; our root signature, pipeline and a live buffer
    // the caller's list holds.
    unsafe {
        l.SetComputeRootSignature(&c.root);
        l.SetPipelineState(&c.pso);
        l.SetComputeRoot32BitConstants(0, 4, constants.as_ptr().cast(), 0);
        l.SetComputeRootUnorderedAccessView(1, gpu);
        l.SetComputeRootUnorderedAccessView(2, budget.GetGPUVirtualAddress());
        l.Dispatch(records.div_ceil(THREADS_PER_GROUP), 1, 1);
    }
    if let Some(enhanced) = &enhanced {
        buffer_barriers(enhanced, written, COMPUTE_UAV, COMMON_ACCESS);
        buffer_barriers(enhanced, separate_count, COMMON_ACCESS, COMMON_ACCESS);
    } else {
        uav_barrier(l);
        // The clamp writes only the arguments. A distinct count can promote
        // from COMMON, following a producer handoff if written in this batch.
        // SAFETY: an open list and a live argument buffer.
        unsafe {
            l.ResourceBarrier(&[transition(
                buffer,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
            )]);
        }
    }
    true
}

/// Finish indirect reads before later passes reuse the buffers. COMMON
/// access leaves them available to shaders, vertex fetch, or another call.
pub(crate) fn done(list: &HostList, args: &ID3D12Resource, count: Option<&ID3D12Resource>) {
    if let Some(enhanced) = enhanced_list(list) {
        let buffers = [Some(args), count.filter(|&count| count != args)]
            .into_iter()
            .flatten();
        buffer_barriers(&enhanced, buffers, COMMON_ACCESS, COMMON_ACCESS);
    } else {
        // SAFETY: bound transitioned the arguments before indirect use.
        unsafe {
            list.list.ResourceBarrier(&[transition(
                args,
                D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
                D3D12_RESOURCE_STATE_COMMON,
            )]);
        }
    }
}

fn uav_barrier(list: &ID3D12GraphicsCommandList) {
    // SAFETY: a global UAV barrier with no resource pointer.
    unsafe {
        list.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
            Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                    pResource: std::mem::ManuallyDrop::new(None),
                }),
            },
        }]);
    }
}

pub(crate) fn buffer_barriers<'a>(
    list: &ID3D12GraphicsCommandList7,
    buffers: impl IntoIterator<Item = &'a ID3D12Resource>,
    (sync_before, access_before): (D3D12_BARRIER_SYNC, D3D12_BARRIER_ACCESS),
    (sync_after, access_after): (D3D12_BARRIER_SYNC, D3D12_BARRIER_ACCESS),
) {
    let barriers: Vec<_> = buffers
        .into_iter()
        .map(|buffer| D3D12_BUFFER_BARRIER {
            SyncBefore: sync_before,
            SyncAfter: sync_after,
            AccessBefore: access_before,
            AccessAfter: access_after,
            // SAFETY: borrow the interface for the duration of the call.
            pResource: unsafe { std::mem::transmute_copy(buffer) },
            Offset: 0,
            Size: u64::MAX,
        })
        .collect();
    if !barriers.is_empty() {
        // SAFETY: the barriers name live whole buffers on an open list.
        unsafe {
            list.Barrier(&[D3D12_BARRIER_GROUP {
                Type: D3D12_BARRIER_TYPE_BUFFER,
                NumBarriers: barriers.len() as u32,
                Anonymous: D3D12_BARRIER_GROUP_0 {
                    pBufferBarriers: barriers.as_ptr(),
                },
            }])
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::Host;

    const COMPUTE: u32 = 2;
    const BUDGET: u32 = 1 << 27;
    const FULL_DISPATCH: [u32; 3] = [1024, 1024, 1];
    const EMPTY_DISPATCH: [u32; 3] = [0, 1024, 1];

    #[test]
    fn record_span_stops_at_the_last_argument() {
        assert_eq!(byte_len(COMPUTE, 16, 2), Some(28));
        assert_eq!(byte_len(COMPUTE, 16, 1), Some(12));
        assert_eq!(byte_len(COMPUTE, 16, 0), Some(0));
        for (kind, size) in [(0, 16), (1, 20), (COMPUTE, 12), (10, 12)] {
            assert_eq!(byte_len(kind, 32, 1), Some(size));
            assert_eq!(byte_len(kind, 32, 2), Some(32 + size));
        }
    }

    #[test]
    fn record_span_bounds() {
        let max = D3D12_CS_DISPATCH_MAX_THREAD_GROUPS_PER_DIMENSION * THREADS_PER_GROUP;
        assert_eq!(byte_len(2, 12, max), Some(u64::from(12 * max)));
        assert_eq!(byte_len(2, 12, max + 1), None);
        assert_eq!(byte_len(2, 12, 0), Some(0));
        assert_eq!(byte_len(2, 1 << 30, 4), Some((3 << 30) + 12));
        assert_eq!(byte_len(2, 1 << 30, 5), None);
        assert_eq!(byte_len(2, u32::MAX - 11, 2), Some(1 << 32));
        assert_eq!(byte_len(2, u32::MAX - 7, 2), None);
        assert_eq!(byte_len(2, u32::MAX - 3, 1), Some(12));
        assert_eq!(byte_len(2, 12, u32::MAX), None);
        assert_eq!(byte_len(0, 12, 1), None);
        assert_eq!(byte_len(1, 16, 1), None);
        assert_eq!(byte_len(10, 0, 1), None);
        assert_eq!(byte_len(2, 13, 1), None);
        assert_eq!(byte_len(3, 12, 1), None);
        assert_eq!(byte_len(1, 24, 2), Some(44));
    }

    /// The clamp compiles with the system's compiler and the host takes it.
    #[test]
    fn argument_clamp_builds() {
        use windows::Win32::Graphics::Dxgi::*;

        let Some(host) = host() else {
            eprintln!("skipped GPU validation: no host D3D12 device");
            return;
        };
        // SAFETY: plain queries on the live device and DXGI factory.
        let desc = unsafe {
            let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).unwrap();
            let adapter: IDXGIAdapter1 = factory
                .EnumAdapterByLuid(host.device.GetAdapterLuid())
                .unwrap();
            adapter.GetDesc1().unwrap()
        };
        let end = desc
            .Description
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(desc.Description.len());
        eprintln!(
            "GPU validation adapter: {}",
            String::from_utf16_lossy(&desc.Description[..end])
        );
        eprintln!("enhanced indirect barriers: {}", enhanced_barriers());
        assert!(clamp().is_some());
    }

    struct Arguments<const N: usize> {
        buffer: ID3D12Resource,
        mapped: *mut u32,
        records: usize,
    }

    impl<const N: usize> Arguments<N> {
        const OFFSET: u64 = 4;

        fn new(host: &Host, input: &[[u32; N]]) -> Self {
            let (buffer, mapped) = host.shared_buffer(size_of_val(input) as u64 + 4).unwrap();
            let mapped = mapped.cast::<u32>();
            // SAFETY: the mapping holds a guard word followed by all records.
            unsafe {
                mapped.write(u32::MAX);
                mapped
                    .add(1)
                    .cast::<[u32; N]>()
                    .copy_from_nonoverlapping(input.as_ptr(), input.len());
            }
            Self {
                buffer,
                mapped,
                records: input.len(),
            }
        }

        fn gpu(&self) -> u64 {
            // SAFETY: the resource is live and starts with a guard word.
            unsafe { self.buffer.GetGPUVirtualAddress() + Self::OFFSET }
        }

        fn record(host: &Host, list: &HostList, kind: u32, input: &[[u32; N]]) -> Self {
            let args = Self::new(host, input);
            assert!(bound(
                list,
                &args.buffer,
                Self::OFFSET,
                None,
                (kind, (4 * N) as u32, input.len() as u32)
            ));
            done(list, &args.buffer, None);
            args
        }

        fn read(&self) -> Vec<[u32; N]> {
            // SAFETY: the buffer is still live and the submission has finished.
            unsafe {
                assert_eq!(self.mapped.read(), u32::MAX);
                std::slice::from_raw_parts(self.mapped.add(1).cast(), self.records).to_vec()
            }
        }
    }

    struct TitleCount {
        resource: crate::object::This,
        data: Box<[u32; 4]>,
        shadow: std::sync::Arc<crate::shadow::Shadow>,
    }

    impl TitleCount {
        fn new(words: [u32; 4]) -> Self {
            let mut data = Box::new(words);
            let address = data.as_mut_ptr() as u64;
            let resource = crate::resource::test_buffer_at(address, 16);
            let shadow = crate::shadow::find(address, 16).unwrap();
            Self {
                resource,
                data,
                shadow,
            }
        }

        fn address(&self) -> u64 {
            self.data.as_ptr() as u64
        }

        fn handoff(
            &self,
            list: &HostList,
            before: D3D12_RESOURCE_STATES,
            after: D3D12_RESOURCE_STATES,
        ) {
            crate::list::test_buffer_handoff(list, self.resource, before.0 as u32, after.0 as u32);
        }
    }

    impl Drop for TitleCount {
        fn drop(&mut self) {
            crate::object::slot::<extern "system" fn(crate::object::This) -> u32>(
                self.resource,
                0x10,
            )(self.resource);
        }
    }

    fn used(list: &HostList) -> u32 {
        let buffer = list.budget().unwrap();
        let mut mapped = std::ptr::null_mut();
        // SAFETY: the submission has finished and the buffer holds a u32.
        unsafe {
            buffer
                .Map(
                    0,
                    Some(&D3D12_RANGE { Begin: 0, End: 4 }),
                    Some(&mut mapped),
                )
                .unwrap();
            let used = mapped.cast::<u32>().read();
            buffer.Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }));
            used
        }
    }

    #[test]
    fn compute_dimensions_and_resubmission() {
        let Some(host) = host() else { return };
        let list = host.new_list().unwrap();
        let args = Arguments::record(
            host,
            &list,
            COMPUTE,
            &[
                [2, 3, 4],
                [17, 0, 3],
                FULL_DISPATCH,
                [1025, 1024, 1],
                [2048, 2048, 1024],
                [65535, 65535, 65535],
                [65536, 1, 1],
                [0, 65536, 1],
            ],
        );
        list.close();
        for _ in 0..2 {
            host.execute(&[&list]);
            let actual = args.read();
            assert_eq!(actual[4], [0, 2048, 1024], "2^32 groups must run empty");
            assert_eq!(
                actual,
                [
                    [2, 3, 4],
                    [17, 0, 3],
                    FULL_DISPATCH,
                    EMPTY_DISPATCH,
                    [0, 2048, 1024],
                    [0, 65535, 65535],
                    [0, 0, 0],
                    [0, 0, 0],
                ]
            );
            assert_eq!(used(&list), (24 + (1 << 20)) * 64);
        }
    }

    #[test]
    fn compute_work_above_the_record_limit_runs_empty() {
        let Some(host) = host() else { return };
        let list = host.new_list().unwrap();
        let args = Arguments::record(host, &list, COMPUTE, &[[1025, 1024, 1]]);
        list.close();
        host.execute(&[&list]);
        assert_eq!(args.read(), [EMPTY_DISPATCH]);
        assert_eq!(used(&list), 0);
    }

    #[test]
    fn rejected_reservations_cannot_wrap_the_budget() {
        let Some(host) = host() else { return };
        let list = host.new_list().unwrap();
        let full = Arguments::record(host, &list, COMPUTE, &[FULL_DISPATCH; 2]);
        let rejected = Arguments::record(host, &list, COMPUTE, &[FULL_DISPATCH; 128]);
        let last = Arguments::record(host, &list, COMPUTE, &[[1, 1, 1]]);
        list.close();
        host.execute(&[&list]);
        assert_eq!(full.read(), [FULL_DISPATCH; 2]);
        let admitted = rejected.read().iter().filter(|g| g[0] != 0).count();
        assert_eq!(admitted, 0);
        assert_eq!(last.read(), [[0, 1, 1]]);
        assert_eq!(used(&list), BUDGET);
    }

    #[test]
    fn rejected_reservation_leaves_room_for_smaller_dispatch() {
        let Some(host) = host() else { return };
        let list = host.new_list().unwrap();
        let _full = Arguments::record(host, &list, COMPUTE, &[FULL_DISPATCH]);
        let _almost = Arguments::record(host, &list, COMPUTE, &[[32767, 32, 1]]);
        let rejected = Arguments::record(host, &list, COMPUTE, &[[33, 1, 1]]);
        let fits = Arguments::record(host, &list, COMPUTE, &[[32, 1, 1]]);
        list.close();
        host.execute(&[&list]);
        assert_eq!(rejected.read(), [[0, 1, 1]]);
        assert_eq!(fits.read(), [[32, 1, 1]]);
        assert_eq!(used(&list), BUDGET);
    }

    #[test]
    fn concurrent_reservations_stay_within_budget() {
        let Some(host) = host() else { return };
        let list = host.new_list().unwrap();
        let args = Arguments::record(host, &list, COMPUTE, &[[128, 128, 1]; 1024]);
        list.close();
        host.execute(&[&list]);
        let mut admitted = 0;
        for record in args.read() {
            match record {
                [128, 128, 1] => admitted += 1,
                [0, 128, 1] => {}
                _ => panic!("unexpected dispatch {record:?}"),
            }
        }
        assert_eq!(admitted * 128 * 128 * 64, BUDGET);
        assert_eq!(used(&list), BUDGET);
    }

    #[test]
    fn draw_and_mesh_limits_are_preserved() {
        let Some(host) = host() else { return };
        let list = host.new_list().unwrap();
        let draw = Arguments::record(host, &list, 0, &[[3, 2, 5, 7], [u32::MAX, u32::MAX, 9, 11]]);
        let indexed = Arguments::record(host, &list, 1, &[[6, 2, 1, u32::MAX, 2]]);
        let mesh = Arguments::record(host, &list, 10, &[[17, 1, 1], [7, 0, 1], [65536, 3, 2]]);
        list.close();
        host.execute(&[&list]);
        assert_eq!(draw.read(), [[3, 2, 5, 7], [1 << 24, 4, 9, 11]]);
        assert_eq!(indexed.read(), [[6, 2, 1, u32::MAX, 2]]);
        assert_eq!(mesh.read(), [[17, 1, 1], [7, 0, 1], [65535, 1, 1]]);
        assert_eq!(used(&list), 6 + (1 << 26) + 12 + (17 + 65535) * 128);
    }

    fn dispatch_signature(host: &Host, stride: u32) -> ID3D12CommandSignature {
        let mut signature: Option<ID3D12CommandSignature> = None;
        let argument = D3D12_INDIRECT_ARGUMENT_DESC {
            Type: D3D12_INDIRECT_ARGUMENT_TYPE_DISPATCH,
            ..Default::default()
        };
        // SAFETY: the signature describes one dispatch with no root arguments.
        unsafe {
            host.device
                .CreateCommandSignature(
                    &D3D12_COMMAND_SIGNATURE_DESC {
                        ByteStride: stride,
                        NumArgumentDescs: 1,
                        pArgumentDescs: &argument,
                        NodeMask: 0,
                    },
                    None::<&ID3D12RootSignature>,
                    &mut signature,
                )
                .unwrap();
        }
        signature.unwrap()
    }

    fn dispatch(
        list: &HostList,
        args: (&ID3D12Resource, u64),
        signature: &ID3D12CommandSignature,
        max: u32,
        count: Option<(&ID3D12Resource, u64)>,
        work: u64,
    ) {
        // Reuse the sanitizer as the executed workload. Each dispatch
        // increments the budget by a known amount.
        let constants = [COMPUTE, 12, 1, 0];
        // SAFETY: inputs are synchronized and live, and the sanitizer
        // pipeline is still bound with the list's budget UAV.
        unsafe {
            list.list
                .SetComputeRoot32BitConstants(0, 4, constants.as_ptr().cast(), 0);
            list.list.SetComputeRootUnorderedAccessView(1, work);
            list.list.ExecuteIndirect(
                signature,
                max,
                args.0,
                args.1,
                count.map(|(buffer, _)| buffer),
                count.map_or(0, |(_, offset)| offset),
            );
        }
        done(list, args.0, count.map(|(buffer, _)| buffer));
    }

    #[test]
    fn native_dispatch_reads_the_last_record_at_the_end_of_the_buffer() {
        let Some(host) = host() else { return };
        let list = host.new_list().unwrap();
        // Two dispatches with a 16-byte stride. From offset 4 the last
        // argument ends exactly at byte 32 of the native resource.
        let args = Arguments::new(host, &[[1, 1, 1, u32::MAX, 1, 1, 1]]);
        let work = Arguments::new(host, &[[6, 2, 3]]);
        let signature = dispatch_signature(host, 16);
        let (short, _) = host.shared_buffer(31).unwrap();
        assert!(!bound(&list, &short, 4, None, (COMPUTE, 16, 2)));
        assert!(bound(&list, &args.buffer, 4, None, (COMPUTE, 16, 2)));
        dispatch(&list, (&args.buffer, 20), &signature, 1, None, work.gpu());
        list.close();
        host.execute(&[&list]);
        assert_eq!(used(&list), (2 + 36) * 64);
        assert_eq!(args.read(), [[1, 1, 1, u32::MAX, 1, 1, 1]]);
    }

    #[test]
    fn cpu_count_is_reused_with_separate_and_shared_buffers() {
        let Some(host) = host() else { return };
        let list = host.new_list().unwrap();
        let args = Arguments::new(host, &[[1, 1, 1, 1, 1, 1, 1]]);
        let count = Arguments::new(host, &[[1]]);
        let copied_count = Arguments::new(host, &[[0]]);
        let work = Arguments::new(host, &[[6, 2, 3]]);
        let signature = dispatch_signature(host, 12);
        // SAFETY: live buffers. The later read of the count can share its
        // legacy COPY_SOURCE state through read-only promotion.
        unsafe {
            list.list
                .CopyBufferRegion(&copied_count.buffer, 4, &count.buffer, 4, 4);
        }
        for (buffer, offset) in [(&count.buffer, 4), (&args.buffer, 28)] {
            assert!(bound(
                &list,
                &args.buffer,
                4,
                Some(buffer),
                (COMPUTE, 12, 2)
            ));
            dispatch(
                &list,
                (&args.buffer, 4),
                &signature,
                2,
                Some((buffer, offset)),
                work.gpu(),
            );
        }
        list.close();
        for _ in 0..2 {
            host.execute(&[&list]);
            assert_eq!(used(&list), 2 * (2 + 36) * 64);
            assert_eq!(copied_count.read(), [[1]]);
        }
    }

    #[test]
    fn gpu_count_producer_in_another_list_and_submission() {
        let Some(host) = host() else { return };
        let reader = host.new_list().unwrap();
        let args = Arguments::new(host, &[[1, 1, 1]; 2]);
        let mut count = TitleCount::new([u32::MAX, 0, 2, 1]);
        let work = Arguments::new(host, &[[6, 2, 3]]);
        let signature = dispatch_signature(host, 12);
        let c = clamp().unwrap();
        let constants = [10u32, 12, 1, 0];
        let record_writer = |count: &TitleCount| {
            let writer = host.new_list().unwrap();
            count.shadow.refresh(&writer, count.address(), 16);
            count.shadow.gpu_writes(&writer, count.address() + 4, 12);
            let budget = writer.budget().unwrap();
            // SAFETY: the mesh clamp stores Y=1 at byte 8 of the count.
            unsafe {
                writer.list.SetComputeRootSignature(&c.root);
                writer.list.SetPipelineState(&c.pso);
                writer
                    .list
                    .SetComputeRoot32BitConstants(0, 4, constants.as_ptr().cast(), 0);
                writer
                    .list
                    .SetComputeRootUnorderedAccessView(1, count.shadow.gpu(count.address() + 4));
                writer
                    .list
                    .SetComputeRootUnorderedAccessView(2, budget.GetGPUVirtualAddress());
                writer.list.Dispatch(1, 1, 1);
            }
            writer
        };
        let writer = record_writer(&count);
        if enhanced_list(&writer).is_none() {
            count.handoff(
                &writer,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
            );
        }
        assert!(bound(
            &reader,
            &args.buffer,
            4,
            Some(&count.shadow.buffer),
            (COMPUTE, 12, 2)
        ));
        dispatch(
            &reader,
            (&args.buffer, 4),
            &signature,
            2,
            Some((&count.shadow.buffer, 8)),
            work.gpu(),
        );
        writer.close();
        reader.close();
        host.execute(&[&writer, &reader]);
        assert_eq!(*count.data, [u32::MAX, 0, 1, 1]);
        assert_eq!(used(&reader), (2 + 36) * 64);
        // In separate submissions the writer's UAV state decays naturally.
        count.data[2] = 2;
        let writer = record_writer(&count);
        writer.close();
        host.execute(&[&writer]);
        assert_eq!(*count.data, [u32::MAX, 0, 1, 1]);
        host.execute(&[&reader]);
        assert_eq!(used(&reader), (2 + 36) * 64);
    }

    #[test]
    fn indirect_buffers_can_be_used_for_copies_before_and_after() {
        let Some(host) = host() else { return };
        let list = host.new_list().unwrap();
        let args = Arguments::new(host, &[[1, 1, 1]; 2]);
        let count = TitleCount::new([u32::MAX, 0, 0, 0]);
        let input = Arguments::new(host, &[[1]]);
        let first_arg = Arguments::new(host, &[[0]]);
        let result = Arguments::new(host, &[[0]]);
        let work = Arguments::new(host, &[[6, 2, 3]]);
        let signature = dispatch_signature(host, 12);
        let enhanced = enhanced_list(&list).is_some();
        if enhanced {
            // SAFETY: the enhanced prelude permits an argument copy read
            // before the clamp.
            unsafe {
                list.list
                    .CopyBufferRegion(&first_arg.buffer, 4, &args.buffer, 4, 4)
            };
        }
        for call in 0..2 {
            if !enhanced && call > 0 {
                count.handoff(
                    &list,
                    D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                );
            }
            // SAFETY: live buffers and a writable copy destination.
            unsafe {
                list.list
                    .CopyBufferRegion(&count.shadow.buffer, 4, &input.buffer, 4, 4)
            };
            if !enhanced {
                count.handoff(
                    &list,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
                );
            }
            assert!(bound(
                &list,
                &args.buffer,
                4,
                Some(&count.shadow.buffer),
                (COMPUTE, 12, 2)
            ));
            dispatch(
                &list,
                (&args.buffer, 4),
                &signature,
                2,
                Some((&count.shadow.buffer, 4)),
                work.gpu(),
            );
        }
        // SAFETY: the count buffer can be read as a copy source after the
        // indirect reads. Legacy read promotions can combine.
        unsafe {
            list.list
                .CopyBufferRegion(&result.buffer, 4, &count.shadow.buffer, 4, 4)
        };
        list.close();
        for _ in 0..2 {
            host.execute(&[&list]);
            assert_eq!(used(&list), 2 * (2 + 36) * 64);
            if enhanced {
                assert_eq!(first_arg.read(), [[1]]);
            }
            assert_eq!(result.read(), [[1]]);
        }
    }
}
