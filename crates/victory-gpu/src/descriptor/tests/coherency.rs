use super::*;
use crate::host::{Host, HostList};
use crate::pipeline::host::HostPipeline;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R32_TYPELESS;
use windows::Win32::Graphics::Dxgi::*;

const SOURCE: &str = r#"
cbuffer Input : register(b0) { uint index; }
RWByteAddressBuffer output_data : register(u0);
[RootSignature("RootFlags(CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED), RootConstants(num32BitConstants=1, b0), UAV(u0)")]
[numthreads(1, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    ByteAddressBuffer data = ResourceDescriptorHeap[index];
    output_data.Store(0, data.Load(0));
}
"#;

struct TestHeap(This);

impl TestHeap {
    fn new(count: u32) -> Self {
        let mut heap = std::ptr::null();
        assert_eq!(
            create(
                HeapDesc {
                    kind: 0,
                    count,
                    flags: 1,
                    nodes: 0
                },
                Placement {
                    address: 0,
                    size: 0
                },
                &mut heap,
            ),
            S_OK,
        );
        Self(heap)
    }

    fn handle(&self, index: u32) -> u64 {
        heap(self.0).start + 32 * u64::from(index)
    }
}

impl Drop for TestHeap {
    fn drop(&mut self) {
        release(self.0);
    }
}

struct TitleBuffer {
    memory: Option<crate::resource::Memory>,
    base: u64,
    size: u64,
}

impl TitleBuffer {
    fn new(size: u64) -> Self {
        let memory = crate::resource::Memory::zeroed(size, crate::watch::PAGE as u64).unwrap();
        let base = memory.address();
        crate::shadow::register(base as usize, base, size);
        Self {
            memory: Some(memory),
            base,
            size,
        }
    }

    #[cfg(feature = "diagnostics")]
    fn watched(size: u64) -> Self {
        use windows_sys::Win32::System::Memory::*;
        const MEM_WRITE_WATCH: u32 = 0x20_0000;
        // SAFETY: a new committed, write-watched synthetic allocation.
        let ptr = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                size as usize,
                MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH,
                PAGE_READWRITE,
            )
        };
        assert!(!ptr.is_null());
        let base = ptr as u64;
        crate::shadow::register(base as usize, base, size);
        Self {
            memory: None,
            base,
            size,
        }
    }

    fn set(&self, offset: u64, value: u32) {
        assert!(offset + 4 <= self.size && offset.is_multiple_of(4));
        // SAFETY: the initialized dword belongs to this title allocation.
        unsafe { ((self.base + offset) as *mut u32).write(value) };
    }

    fn srv(&self, handle: u64, offset: u64, len: u32) {
        let mut resource = [0; 14];
        resource[0] = 1;
        resource[4] = self.size as u32;
        let address = self.base;
        let mut view = [0; 16];
        view[0] = DXGI_FORMAT_R32_TYPELESS.0 as u32;
        view[1] = D3D12_SRV_DIMENSION_BUFFER.0 as u32;
        view[2] = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
        view[4] = (offset / 4) as u32;
        view[6] = len / 4;
        view[8] = D3D12_BUFFER_SRV_FLAG_RAW.0 as u32;
        view[10] = address as u32;
        view[11] = (address >> 32) as u32;
        create_placed_srv(std::ptr::null(), &resource, &view, handle);
    }
}

impl Drop for TitleBuffer {
    fn drop(&mut self) {
        crate::shadow::unregister(self.base as usize, self.base);
        if self.memory.is_none() {
            use windows_sys::Win32::System::Memory::{MEM_RELEASE, VirtualFree};
            // SAFETY: this was allocated with VirtualAlloc.
            unsafe { VirtualFree(self.base as *mut _, 0, MEM_RELEASE) };
        }
    }
}

fn pipeline(host: &Host, dxc: &std::ffi::OsStr) -> HostPipeline {
    compile(host, dxc, SOURCE)
}

fn compile(host: &Host, dxc: &std::ffi::OsStr, hlsl: &str) -> HostPipeline {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("coherency.hlsl");
    let output = temp.path().join("coherency.dxil");
    std::fs::write(&source, hlsl).unwrap();
    let built = std::process::Command::new(dxc)
        .args(["-T", "cs_6_6", "-E", "main", "-Fo"])
        .arg(&output)
        .arg(&source)
        .output()
        .unwrap();
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    let bytes = std::fs::read(&output).unwrap();
    // SAFETY: the DXIL blob and signature stay alive during pipeline creation.
    let (root, pso) = unsafe {
        let root = host
            .device
            .CreateRootSignature::<ID3D12RootSignature>(0, &bytes)
            .unwrap();
        let pso = host
            .device
            .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: bytes.as_ptr().cast(),
                    BytecodeLength: bytes.len(),
                },
                ..Default::default()
            })
            .unwrap();
        (root, pso)
    };
    HostPipeline {
        compute: true,
        mesh: false,
        pso,
        root,
        stages: Vec::new(),
        slots: 0,
        inputs: Vec::new(),
        name: "synthetic direct SRV".into(),
    }
}

fn test_host() -> Option<(&'static Host, std::ffi::OsString)> {
    let Some(dxc) = std::env::var_os("VICTORY_PC_DXC") else {
        eprintln!("skipped direct SRV GPU test: VICTORY_PC_DXC unset");
        return None;
    };
    let host = crate::host::host().expect("D3D12 adapter for direct SRV GPU test");
    let mut shader_model = D3D12_FEATURE_DATA_SHADER_MODEL {
        HighestShaderModel: D3D_SHADER_MODEL_6_6,
    };
    let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
    // SAFETY: feature query and adapter descriptor write the supplied structs.
    let adapter = unsafe {
        host.device
            .CheckFeatureSupport(
                D3D12_FEATURE_SHADER_MODEL,
                (&raw mut shader_model).cast(),
                size_of_val(&shader_model) as u32,
            )
            .unwrap();
        host.device
            .CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS,
                (&raw mut options).cast(),
                size_of_val(&options) as u32,
            )
            .unwrap();
        let factory = CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0)).unwrap();
        factory
            .EnumAdapterByLuid::<IDXGIAdapter1>(host.device.GetAdapterLuid())
            .unwrap()
            .GetDesc1()
            .unwrap()
    };
    assert!(
        shader_model.HighestShaderModel.0 >= D3D_SHADER_MODEL_6_6.0,
        "SM 6.6 required"
    );
    assert_eq!(options.ResourceBindingTier, D3D12_RESOURCE_BINDING_TIER_3);
    let end = adapter
        .Description
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(adapter.Description.len());
    eprintln!(
        "direct SRV GPU test on {}, SM 6.6",
        String::from_utf16_lossy(&adapter.Description[..end])
    );
    Some((host, dxc))
}

fn record(
    list: &HostList,
    heap: This,
    pipeline: &HostPipeline,
    index: u32,
    output: &ID3D12Resource,
) {
    let mut state = crate::draw::State::default();
    state.set_heaps(&[heap]);
    state.bind(list, pipeline, true);
    // SAFETY: the open list holds its heap and pipeline. The test keeps output alive.
    unsafe {
        list.list.SetComputeRoot32BitConstant(0, index, 0);
        list.list
            .SetComputeRootUnorderedAccessView(1, output.GetGPUVirtualAddress());
        list.list.Dispatch(1, 1, 1);
    }
}

#[test]
fn directly_indexed_srv_reads_cpu_changes_without_remirroring() {
    let Some((host, dxc)) = test_host() else {
        return;
    };
    let pipeline = pipeline(host, &dxc);
    let title = TitleBuffer::new(65536);
    title.set(0, 13);
    let heap_owner = TestHeap::new(64);
    let handle = heap_owner.handle(17);
    title.srv(handle, 0, 4);
    let heap = heap(heap_owner.0).host().unwrap();
    // Prime the same slot and copy the first word as a conventional table use would.
    heap.mirror(17, handle, true).unwrap();
    let initial = host.new_list().unwrap();
    let address = title.base;
    crate::shadow::find(address, title.size)
        .unwrap()
        .refresh(&initial, address, 4);
    let (output, mapped) = host.shared_buffer(4).unwrap();
    record(&initial, heap_owner.0, &pipeline, 17, &output);
    initial.close();
    host.execute(&[&initial]);
    // SAFETY: the queue has completed and output is still mapped.
    assert_eq!(unsafe { mapped.cast::<u32>().read() }, 13);

    let direct = host.new_list().unwrap();
    record(&direct, heap_owner.0, &pipeline, 17, &output);
    direct.close();
    title.set(0, 500);
    host.execute(&[&direct]);
    // SAFETY: the queue has completed and output is still mapped.
    assert_eq!(unsafe { mapped.cast::<u32>().read() }, 500);
}

fn read(mapped: *mut u8) -> u32 {
    // SAFETY: tests call after the submission returns while the buffer lives.
    unsafe { mapped.cast::<u32>().read() }
}

#[test]
fn directly_indexed_srv_copies_and_replaces_descriptors_before_submit() {
    let Some((host, dxc)) = test_host() else {
        return;
    };
    let pipeline = pipeline(host, &dxc);
    let a = TitleBuffer::new(65536);
    let b = TitleBuffer::new(65536);
    let heap = TestHeap::new(64);
    let (output, mapped) = host.shared_buffer(4).unwrap();
    let list = host.new_list().unwrap();
    record(&list, heap.0, &pipeline, 15, &output);
    list.close();
    a.set(0, 47);
    a.srv(heap.handle(2), 0, 4);
    copy_simple(std::ptr::null(), 1, heap.handle(15), heap.handle(2), 0);
    host.execute(&[&list]);
    assert_eq!(read(mapped), 47);

    b.set(0, 81);
    b.srv(heap.handle(15), 0, 4);
    drop(heap);
    host.execute(&[&list]);
    assert_eq!(read(mapped), 81);
}

#[test]
fn directly_indexed_srv_fixed_slot_first_use() {
    let Some((host, dxc)) = test_host() else {
        return;
    };
    let pipeline = compile(host, &dxc, &SOURCE.replace("Heap[index]", "Heap[17]"));
    let title = TitleBuffer::new(65536);
    title.set(0, 66);
    let heap = TestHeap::new(64);
    title.srv(heap.handle(17), 0, 4);
    let (output, mapped) = host.shared_buffer(4).unwrap();
    let list = host.new_list().unwrap();
    record(&list, heap.0, &pipeline, 0, &output);
    list.close();
    host.execute(&[&list]);
    assert_eq!(read(mapped), 66);
}

#[test]
fn directly_indexed_srv_ignores_released_resource_generations() {
    let Some((host, dxc)) = test_host() else {
        return;
    };
    let pipeline = pipeline(host, &dxc);
    let title = TitleBuffer::new(65536);
    crate::shadow::unregister(title.base as usize, title.base);
    let resource = crate::resource::test_buffer_at(title.base, title.size);
    let heap = TestHeap::new(64);
    let mut view = [0; 10];
    view[0] = DXGI_FORMAT_R32_TYPELESS.0 as u32;
    view[1] = D3D12_SRV_DIMENSION_BUFFER.0 as u32;
    view[2] = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
    view[6] = 1;
    view[8] = D3D12_BUFFER_SRV_FLAG_RAW.0 as u32;
    create_srv(std::ptr::null(), resource, &view, heap.handle(15));
    let (output, mapped) = host.shared_buffer(4).unwrap();
    let list = host.new_list().unwrap();
    record(&list, heap.0, &pipeline, 15, &output);
    list.close();
    title.set(0, 19);
    host.execute(&[&list]);
    assert_eq!(read(mapped), 19);

    let release = |resource| slot::<extern "system" fn(This) -> u32>(resource, 0x10)(resource);
    assert_eq!(release(resource), 0);
    let replacement = crate::resource::test_buffer_at(title.base, title.size);
    title.set(0, 82);
    host.execute(&[&list]);
    assert_eq!(read(mapped), 19);
    create_srv(std::ptr::null(), replacement, &view, heap.handle(15));
    host.execute(&[&list]);
    assert_eq!(read(mapped), 82);
    assert_eq!(release(replacement), 0);
}

#[test]
fn directly_indexed_srv_sees_producer_in_same_submission() {
    let Some((host, dxc)) = test_host() else {
        return;
    };
    let pipeline = pipeline(host, &dxc);
    let title = TitleBuffer::new(65536);
    title.set(0, 41);
    let heap = TestHeap::new(64);
    title.srv(heap.handle(1), 0, 4);
    let shadow = crate::shadow::find(title.base, title.size).unwrap();
    let (input, source) = host.shared_buffer(4).unwrap();
    let (output, mapped) = host.shared_buffer(4).unwrap();
    let producer = host.new_list().unwrap();
    let consumer = host.new_list().unwrap();
    // SAFETY: live mappings and lists. The test explicitly orders the native producer.
    unsafe {
        source.cast::<u32>().write(77);
        producer
            .list
            .CopyBufferRegion(&shadow.buffer, 0, &input, 0, 4);
        producer.list.ResourceBarrier(&[crate::host::transition(
            &shadow.buffer,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
        )]);
    }
    producer.close();
    record(&consumer, heap.0, &pipeline, 1, &output);
    consumer.close();
    host.execute(&[&producer, &consumer]);
    assert_eq!(read(mapped), 77);
}

/// Small views into one page of a buffer, as the world's directly indexed
/// SRVs are, refresh as one range, and each still reads its own bytes.
#[cfg(feature = "diagnostics")]
#[test]
fn directly_indexed_srvs_in_one_page_refresh_as_one_range() {
    let Some((host, dxc)) = test_host() else {
        return;
    };
    use crate::copystat::{C, read as counter};
    const VIEWS: u32 = 16;
    const STRIDE: u64 = 4096;
    let pipeline = pipeline(host, &dxc);
    let title = TitleBuffer::new(65536);
    let heap = TestHeap::new(64);
    for i in 0..VIEWS {
        title.set(u64::from(i) * STRIDE, 100 + i);
        title.srv(heap.handle(i), u64::from(i) * STRIDE, 16);
    }
    let (output, mapped) = host.shared_buffer(4).unwrap();
    for i in [0, VIEWS / 2, VIEWS - 1] {
        let list = host.new_list().unwrap();
        record(&list, heap.0, &pipeline, i, &output);
        list.close();
        let before = counter(C::DirectRanges);
        host.execute(&[&list]);
        assert_eq!(counter(C::DirectRanges) - before, 1);
        assert_eq!(read(mapped), 100 + i);
    }
}

#[cfg(feature = "diagnostics")]
#[test]
fn directly_indexed_srv_unwatched_range_preserves_watched_refreshes() {
    let Some((host, dxc)) = test_host() else {
        return;
    };
    use crate::copystat::{C, read as counter};

    const WATCHED: usize = 16 << 20;
    const PAGE: usize = crate::watch::PAGE;
    const CLEAN_RUNS: usize = 4;
    const LEGACY_SOURCE: &str = r#"
ByteAddressBuffer data : register(t0);
RWByteAddressBuffer output_data : register(u0);
[RootSignature("SRV(t0), UAV(u0)")]
[numthreads(1, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    output_data.Store(0, data.Load(0));
}
"#;

    struct TitleResource(This);
    impl Drop for TitleResource {
        fn drop(&mut self) {
            slot::<extern "system" fn(This) -> u32>(self.0, 0x10)(self.0);
        }
    }

    let (tracked, ordinary) = crate::watch::tests::adjacent(WATCHED, PAGE);
    let base = tracked.0 as u64;
    let resource = TitleResource(crate::resource::test_buffer_at(
        base,
        (WATCHED + PAGE) as u64,
    ));
    // SAFETY: the test owns the two committed title allocations. Touch all watched
    // pages before refreshing so demand-zero faults do not skew the clean phase.
    unsafe {
        tracked.0.cast::<u8>().write_bytes(0, WATCHED);
        tracked.0.cast::<u32>().write(13);
        ordinary.0.cast::<u32>().write(27);
    }

    let shadow = crate::shadow::find(base, WATCHED as u64).unwrap();
    let conventional = host.new_list().unwrap();
    shadow.refresh(&conventional, base, WATCHED as u64);
    let legacy = compile(host, &dxc, LEGACY_SOURCE);
    crate::draw::State::default().bind(&conventional, &legacy, true);
    let (output, conventional_result) = host.shared_buffer(4).unwrap();
    // SAFETY: the resources and pipeline stay alive until this list completes.
    unsafe {
        conventional
            .list
            .SetComputeRootShaderResourceView(0, shadow.gpu(base));
        conventional
            .list
            .SetComputeRootUnorderedAccessView(1, output.GetGPUVirtualAddress());
        conventional.list.Dispatch(1, 1, 1);
    }
    conventional.close();
    host.execute(&[&conventional]);
    assert_eq!(read(conventional_result), 13);

    let before = counter(C::ConventionalBytes);
    host.execute(&[&conventional]);
    assert_eq!(counter(C::ConventionalBytes) - before, 0);

    let direct_pipeline = pipeline(host, &dxc);
    let heap = TestHeap::new(64);
    let mut view = [0; 10];
    view[0] = DXGI_FORMAT_R32_TYPELESS.0 as u32;
    view[1] = D3D12_SRV_DIMENSION_BUFFER.0 as u32;
    view[2] = D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING;
    view[4] = (WATCHED / 4) as u32;
    view[6] = 1;
    view[8] = D3D12_BUFFER_SRV_FLAG_RAW.0 as u32;
    create_srv(std::ptr::null(), resource.0, &view, heap.handle(17));
    let (output, direct_result) = host.shared_buffer(4).unwrap();
    let direct = host.new_list().unwrap();
    record(&direct, heap.0, &direct_pipeline, 17, &output);
    direct.close();
    let before = counter(C::DirectBytes);
    host.execute(&[&direct]);
    let direct_bytes = counter(C::DirectBytes) - before;
    assert_eq!(read(direct_result), 27);
    assert_eq!(direct_bytes, 4);

    let before = counter(C::ConventionalBytes);
    let started = std::time::Instant::now();
    for _ in 0..CLEAN_RUNS {
        host.execute(&[&conventional]);
    }
    let clean_bytes = counter(C::ConventionalBytes) - before;
    let clean_us = started.elapsed().as_micros();
    // SAFETY: this dword is in the live write-watched reservation.
    unsafe { tracked.0.cast::<u32>().write(500) };
    let before = counter(C::ConventionalBytes);
    host.execute(&[&conventional]);
    let dirty_bytes = counter(C::ConventionalBytes) - before;
    assert_eq!(read(conventional_result), 500);
    eprintln!(
        "mixed-reservation SRV: direct_bytes={direct_bytes} clean_submits={CLEAN_RUNS} \
         clean_conventional_requested={} clean_conventional_bytes={clean_bytes} wall_us={clean_us} \
         dirty_conventional_bytes={dirty_bytes} gpu_direct={} gpu_conventional={}",
        WATCHED * CLEAN_RUNS,
        read(direct_result),
        read(conventional_result),
    );
    assert_eq!(clean_bytes, 0);
    assert_eq!(dirty_bytes, PAGE as u64);
}

#[cfg(feature = "diagnostics")]
#[test]
fn directly_indexed_srv_sparse_heap_benchmark() {
    if std::env::var_os("VICTORY_COH_BENCH").is_none() {
        return;
    }
    let Some((host, dxc)) = test_host() else {
        return;
    };
    use crate::copystat::{C, read as counter};
    const PAGE: u64 = 65536;
    const RUNS: usize = 64;
    const BUFFERS: usize = 32;
    const SLOTS_PER_BUFFER: usize = 32;
    let pipeline = pipeline(host, &dxc);
    let heap = TestHeap::new(1_000_000);
    let titles: Vec<_> = (0..BUFFERS)
        .map(|_| TitleBuffer::watched(2 * PAGE))
        .collect();
    let warm = host.new_list().unwrap();
    let real = super::heap(heap.0).host().unwrap();
    for (i, title) in titles.iter().enumerate() {
        title.set(0, 13);
        for alias in 0..SLOTS_PER_BUFFER {
            let index = (i * SLOTS_PER_BUFFER + alias) as u32;
            let handle = heap.handle(index);
            title.srv(handle, 0, (2 * PAGE) as u32);
            real.mirror(index, handle, true).unwrap();
        }
        crate::shadow::find(title.base, title.size)
            .unwrap()
            .refresh(&warm, title.base, title.size);
    }
    let (output, mapped) = host.shared_buffer(4).unwrap();
    record(&warm, heap.0, &pipeline, 0, &output);
    warm.close();
    host.execute(&[&warm]);
    assert_eq!(read(mapped), 13);
    let lists: Vec<_> = (0..BUFFERS)
        .map(|i| {
            let list = host.new_list().unwrap();
            record(
                &list,
                heap.0,
                &pipeline,
                (i * SLOTS_PER_BUFFER) as u32,
                &output,
            );
            list.close();
            list
        })
        .collect();
    let before = [
        counter(C::DirectSlots),
        counter(C::DirectRanges),
        counter(C::DirectBytes),
        counter(C::DirectUs),
    ];
    let (prep_before, back_before) = crate::host::test_submission_counters();
    let started = std::time::Instant::now();
    for i in 0..RUNS {
        let n = i % BUFFERS;
        titles[n].set(0, i as u32 + 500);
        host.execute(&[&lists[n]]);
    }
    let elapsed = started.elapsed();
    let (prep_after, back_after) = crate::host::test_submission_counters();
    let after = [
        counter(C::DirectSlots),
        counter(C::DirectRanges),
        counter(C::DirectBytes),
        counter(C::DirectUs),
    ];
    let [slots, ranges, bytes, sync_us] = std::array::from_fn(|i| after[i] - before[i]);
    eprintln!(
        "sparse SRV bench: {RUNS} runs, slots={slots}, merged={ranges}, copied={bytes}, refresh_us={sync_us}, prep_us={}, copyback={}, elapsed_us={}, output={}",
        prep_after - prep_before,
        back_after - back_before,
        elapsed.as_micros(),
        read(mapped)
    );
    if std::env::var_os("VICTORY_COH_BASELINE").is_none() {
        assert_eq!(read(mapped), 563);
        assert_eq!(slots, (RUNS * BUFFERS * SLOTS_PER_BUFFER) as u64);
        assert_eq!(ranges, (RUNS * BUFFERS) as u64);
        assert_eq!(bytes, RUNS as u64 * PAGE);
        let copied = counter(C::DirectBytes);
        host.execute(&[&lists[BUFFERS - 1]]);
        assert_eq!(
            counter(C::DirectBytes),
            copied,
            "clean watched pages require no transfer"
        );
    }
}

#[cfg(feature = "diagnostics")]
#[test]
fn directly_indexed_srv_mixed_heap_benchmark() {
    if std::env::var_os("VICTORY_COH_MIXED_BENCH").is_none() {
        return;
    }
    let Some((host, dxc)) = test_host() else {
        return;
    };
    use crate::copystat::{C, read as counter};

    const PAGE: u64 = crate::watch::PAGE as u64;
    const BUFFERS: usize = 192;
    const CONVENTIONAL: usize = 144;
    const SLOTS_PER_BUFFER: usize = 128;
    const LISTS: usize = 4;
    const RUNS: usize = 12;
    const DISPATCHES_PER_LIST: u32 = 40;
    const RANGE_LEN: u32 = 512;
    const HEAP_CAPACITY: u32 = 262_144;
    const SOURCE: &str = r#"
cbuffer Input : register(b0) { uint index; uint result_index; }
ByteAddressBuffer conventional_data : register(t0);
RWByteAddressBuffer output_data : register(u0);
[RootSignature("RootFlags(CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED), RootConstants(num32BitConstants=2, b0), DescriptorTable(SRV(t0)), UAV(u0)")]
[numthreads(1, 1, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    ByteAddressBuffer data = ResourceDescriptorHeap[index];
    output_data.Store2(8 * result_index, uint2(data.Load(0), conventional_data.Load(0)));
}
"#;

    let mode = std::env::var("VICTORY_COH_MIXED_MEMORY").unwrap_or_else(|_| "mixed".into());
    let watched = |i| match mode.as_str() {
        "mixed" => i % 2 == 0,
        "watched" => true,
        "unwatched" => false,
        _ => panic!("unknown VICTORY_COH_MIXED_MEMORY: {mode}"),
    };
    let slot = |buffer, within| ((buffer * SLOTS_PER_BUFFER + within) * 8) as u32;
    let pipeline = compile(host, &dxc, SOURCE);
    let heap = TestHeap::new(HEAP_CAPACITY);
    let native = super::heap(heap.0).host().unwrap();
    let titles: Vec<_> = (0..BUFFERS)
        .map(|i| {
            if watched(i) {
                TitleBuffer::watched(2 * PAGE)
            } else {
                TitleBuffer::new(2 * PAGE)
            }
        })
        .collect();
    let initial_bytes = crate::host::test_submission_copy_bytes();
    let initial_allocations = counter(C::ShadowAllocations);
    let initial_allocation_bytes = counter(C::ShadowAllocationBytes);
    let initial_materializations = counter(C::SrvMaterializations);
    let started = std::time::Instant::now();
    let warm = host.new_list().unwrap();
    for (i, title) in titles.iter().enumerate() {
        if watched(i) {
            // Touch every committed page before the first watch query. The initial
            // read of demand-zero backing can appear as a write on Windows.
            // SAFETY: this test owns the initialized allocation.
            unsafe { (title.base as *mut u8).write_bytes(0, title.size as usize) };
        }
        title.set(0, 13);
        for local in 0..SLOTS_PER_BUFFER {
            // Four descriptors form three disjoint intervals, one overlapping.
            let section = (local / 4 * 3 + (local % 4).min(2)) as u64;
            let offset = section * 1024 + if local % 4 == 3 { 128 } else { 0 };
            let index = slot(i, local);
            let handle = heap.handle(index);
            title.srv(handle, offset, RANGE_LEN);
            native.mirror(index, handle, true).unwrap();
        }
        crate::shadow::find(title.base, title.size)
            .unwrap()
            .refresh(&warm, title.base, title.size);
    }
    warm.close();
    host.execute(&[&warm]);
    eprintln!(
        "mixed SRV setup memory={mode} shadow_allocations={} shadow_bytes={} \
         materialized_descriptors={} populate_bytes={} setup_us={}",
        counter(C::ShadowAllocations) - initial_allocations,
        counter(C::ShadowAllocationBytes) - initial_allocation_bytes,
        counter(C::SrvMaterializations) - initial_materializations,
        crate::host::test_submission_copy_bytes() - initial_bytes,
        started.elapsed().as_micros(),
    );

    // All slots start populated in both cases. The control records identical native
    // GPU work and conventional refreshes without registering the direct heap.
    let control = std::env::var("VICTORY_COH_MIXED_CONTROL").as_deref() == Ok("1");
    let lists: Vec<_> = (0..LISTS)
        .map(|i| {
            let list = host.new_list().unwrap();
            let (output, mapped) = host
                .shared_buffer(u64::from(DISPATCHES_PER_LIST) * 8)
                .unwrap();
            for buffer in 0..CONVENTIONAL {
                for local in [0, 3, 64, 127] {
                    let index = slot(buffer, local);
                    let view = native.mirror(index, heap.handle(index), true).unwrap();
                    let (address, len) = crate::descriptor::host::buffer_range(&view).unwrap();
                    crate::shadow::find(address, len)
                        .unwrap()
                        .refresh(&list, address, len);
                }
            }
            if !control {
                let mut state = crate::draw::State::default();
                state.set_heaps(&[heap.0]);
                state.bind(&list, &pipeline, true);
            }
            // SAFETY: an open list with a live heap, pipeline and output retained by the test.
            unsafe {
                let commands = &list.list;
                if control {
                    commands.SetDescriptorHeaps(&[Some(native.heap.clone())]);
                    commands.SetComputeRootSignature(&pipeline.root);
                    commands.SetPipelineState(&pipeline.pso);
                }
                let first = native.heap.GetGPUDescriptorHandleForHeapStart().ptr;
                let step = host
                    .device
                    .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
                commands.SetComputeRootDescriptorTable(
                    1,
                    D3D12_GPU_DESCRIPTOR_HANDLE {
                        ptr: first + u64::from(slot(i, 0)) * u64::from(step),
                    },
                );
                commands.SetComputeRoot32BitConstant(0, slot(CONVENTIONAL + i, 0), 0);
                commands.SetComputeRootUnorderedAccessView(2, output.GetGPUVirtualAddress());
                for dispatch in 0..DISPATCHES_PER_LIST {
                    commands.SetComputeRoot32BitConstant(0, dispatch, 1);
                    commands.Dispatch(1, 1, 1);
                }
            }
            list.close();
            (list, output, mapped)
        })
        .collect();
    let refs: Vec<_> = lists.iter().map(|(list, _, _)| list).collect();
    let sample = || {
        let (prep, back) = crate::host::test_submission_counters();
        [
            counter(C::DirectSlots),
            counter(C::DirectRanges),
            counter(C::DirectRequested),
            counter(C::DirectBytes),
            counter(C::DirectUs),
            counter(C::Refreshes),
            counter(C::RefreshBytes),
            counter(C::ConventionalRanges),
            counter(C::ConventionalRequested),
            counter(C::ConventionalBytes),
            counter(C::ConventionalUs),
            counter(C::Checked),
            counter(C::Unchanged),
            counter(C::ShadowAllocations),
            counter(C::SrvMaterializations),
            prep,
            back,
            crate::host::test_submission_copy_bytes(),
        ]
    };

    for dirty in [false, true] {
        let before = sample();
        let started = std::time::Instant::now();
        for run in 0..RUNS {
            if dirty {
                titles[0].set(0, 500 + run as u32);
                titles[CONVENTIONAL].set(0, 500 + run as u32);
            }
            host.execute(&refs);
        }
        let wall_us = started.elapsed().as_micros();
        let after = sample();
        let [
            slots,
            direct_ranges,
            direct_requested,
            direct,
            direct_us,
            conventional_calls,
            conventional_scheduled,
            conventional_ranges,
            conventional_requested,
            conventional,
            conventional_us,
            watched_checked,
            watched_unchanged,
            shadow_allocations,
            srv_materializations,
            prep_us,
            copyback,
            total,
        ] = std::array::from_fn(|i| after[i] - before[i]);
        let (_, _, mapped) = lists[0];
        let gpu_direct = read(mapped);
        // SAFETY: the validated shader wrote two dwords to the live output mapping.
        let gpu_conventional = read(unsafe { mapped.add(4) });
        let expected = if dirty { 500 + RUNS as u32 - 1 } else { 13 };
        assert_eq!(gpu_direct, if control { 13 } else { expected });
        assert_eq!(gpu_conventional, expected);
        assert_eq!(conventional_calls, (RUNS * LISTS * CONVENTIONAL * 4) as u64);
        assert_eq!(total, conventional + direct);
        assert_eq!((shadow_allocations, srv_materializations), (0, 0));
        let watched_copied = watched_checked - watched_unchanged;
        eprintln!(
            "mixed SRV bench memory={mode} control={control} dirty={dirty} runs={RUNS} lists={LISTS} \
             dispatches={} shadows={BUFFERS} logical_shadow_bytes={} heap_slots={HEAP_CAPACITY} populated_slots={} \
             slots={slots} direct_ranges={direct_ranges} conventional_calls={conventional_calls} \
             conventional_scheduled={conventional_scheduled} conventional_ranges={conventional_ranges} \
             conventional_requested={conventional_requested} conventional_bytes={conventional} conventional_us={conventional_us} \
             direct_requested={direct_requested} direct_bytes={direct} direct_us={direct_us} \
             prep_bytes={total} prep_us={prep_us} \
             watched_checked={watched_checked} watched_bytes={watched_copied} unwatched_bytes={} \
             copyback={copyback} wall_us={wall_us} gpu_direct={gpu_direct} gpu_conventional={gpu_conventional}",
            RUNS * LISTS * DISPATCHES_PER_LIST as usize,
            BUFFERS as u64 * 2 * PAGE,
            BUFFERS * SLOTS_PER_BUFFER,
            total - watched_copied,
        );
    }

    // Check a GPU-produced input through the same recorded lists, outside the timings.
    let title = &titles[CONVENTIONAL];
    let shadow = crate::shadow::find(title.base, 4).unwrap();
    let producer = host.new_list().unwrap();
    let (input, source) = host.shared_buffer(4).unwrap();
    shadow.gpu_writes(&producer, title.base, 4);
    // SAFETY: live mappings and lists, with the explicit copy-to-SRV transition.
    unsafe {
        source.cast::<u32>().write(733);
        producer
            .list
            .CopyBufferRegion(&shadow.buffer, 0, &input, 0, 4);
        producer.list.ResourceBarrier(&[crate::host::transition(
            &shadow.buffer,
            D3D12_RESOURCE_STATE_COPY_DEST,
            D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
        )]);
    }
    producer.close();
    host.execute(&[&producer, refs[0]]);
    assert_eq!(read(lists[0].2), 733);
    assert_eq!(read(title.base as *mut u8), 733);
}
