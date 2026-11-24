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
