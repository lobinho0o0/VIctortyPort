use super::*;
use crate::host::{Host, HostList};
use crate::object::{guid, slot};
use crate::resource::{HeapProperties, ResourceDesc};
use crate::shadow::{self, Shadow};
use std::sync::{Arc, Mutex, Once};
use windows::Win32::Graphics::Direct3D::{Fxc::D3DCompile, ID3DBlob};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::core::{Interface, PCSTR, s};

const RESOURCE_IID: Iid = guid(
    0x696442be,
    0xa72e,
    0x4059,
    [0xbc, 0x79, 0x5b, 0x5c, 0x98, 0x04, 0x0f, 0xad],
);
const SOURCE: &str = r#"
cbuffer Fill : register(b0) { uint value; }
ByteAddressBuffer source : register(t0);
RWByteAddressBuffer target : register(u0);
[RootSignature("RootConstants(num32BitConstants=1, b0), UAV(u0)")]
[numthreads(1, 1, 1)]
void write_word() { target.Store(0, value); }

[RootSignature("SRV(t0), UAV(u0)")]
[numthreads(1, 1, 1)]
void read_word() { target.Store(0, source.Load(0)); }
"#;

const UAV: D3D12_RESOURCE_STATES = D3D12_RESOURCE_STATE_UNORDERED_ACCESS;
const SRV: D3D12_RESOURCE_STATES = D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE;
const COPY_IN: D3D12_RESOURCE_STATES = D3D12_RESOURCE_STATE_COPY_DEST;
const COPY_OUT: D3D12_RESOURCE_STATES = D3D12_RESOURCE_STATE_COPY_SOURCE;

struct TitleBuffer {
    resource: This,
    address: u64,
    len: u64,
}

impl TitleBuffer {
    fn new(words: &[u32]) -> Self {
        let len = size_of_val(words) as u64;
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            CreationNodeMask: 1,
            VisibleNodeMask: 1,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: len,
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ..Default::default()
        };
        const _: () = assert!(size_of::<HeapProperties>() == size_of::<D3D12_HEAP_PROPERTIES>());
        const _: () = assert!(size_of::<ResourceDesc>() == size_of::<D3D12_RESOURCE_DESC>());
        let mut resource = std::ptr::null();
        assert_eq!(
            crate::resource::create_committed(
                std::ptr::null(),
                (&raw const heap).cast(),
                0,
                (&raw const desc).cast(),
                0,
                std::ptr::null(),
                &RESOURCE_IID,
                &mut resource,
            ),
            S_OK
        );
        let (data, size) = crate::resource::buffer_bytes(resource).unwrap();
        assert_eq!(size, len);
        // SAFETY: the newly allocated resource owns size bytes.
        unsafe {
            data.cast::<u32>()
                .copy_from_nonoverlapping(words.as_ptr(), words.len())
        };
        Self {
            resource,
            address: data as u64,
            len,
        }
    }

    fn set(&self, word: usize, value: u32) {
        assert!(word < self.len as usize / size_of::<u32>());
        // SAFETY: the resource owns the selected word, with no submission in flight.
        unsafe { (self.address as *mut u32).add(word).write(value) };
    }

    fn words(&self) -> Vec<u32> {
        // SAFETY: the resource owns these initialized words, with no submission in flight.
        unsafe {
            std::slice::from_raw_parts(
                self.address as *const u32,
                self.len as usize / size_of::<u32>(),
            )
            .to_vec()
        }
    }

    fn shadow(&self, list: &HostList, word: usize, written: bool) -> Arc<Shadow> {
        let address = self.address + (word * size_of::<u32>()) as u64;
        let shadow = shadow::find(address, 4).unwrap();
        shadow.refresh(list, address, 4);
        if written {
            shadow.gpu_writes(list, address, 4);
        }
        shadow
    }

    fn gpu(&self, shadow: &Shadow, word: usize) -> u64 {
        shadow.gpu(self.address + (word * size_of::<u32>()) as u64)
    }
}

impl Drop for TitleBuffer {
    fn drop(&mut self) {
        slot::<extern "system" fn(This) -> u32>(self.resource, 0x10)(self.resource);
    }
}

struct TitleList {
    list: This,
    allocator: This,
}

impl TitleList {
    fn new() -> Self {
        let null = std::ptr::null();
        let mut allocator = null;
        assert_eq!(
            create_allocator(null, 0, &ALLOCATOR_IIDS[0], &mut allocator),
            S_OK
        );
        let mut list = null;
        assert_eq!(
            create_list(null, 0, 0, allocator, null, &LIST_IIDS[1], &mut list),
            S_OK
        );
        Self { list, allocator }
    }

    fn host(&self) -> &HostList {
        host_list(self.list).expect("native command list")
    }

    fn recording(&self) -> RecordingList<'_> {
        recording_list(self.list).expect("native recording command list")
    }

    fn copy(
        &self,
        dst: &TitleBuffer,
        dst_offset: u64,
        src: &TitleBuffer,
        src_offset: u64,
        size: u64,
    ) {
        copy_buffer_region(
            self.list,
            dst.resource,
            dst_offset,
            src.resource,
            src_offset,
            size,
        );
    }

    fn copy_resource(&self, dst: &TitleBuffer, src: &TitleBuffer) {
        copy_resource(self.list, dst.resource, src.resource);
    }

    fn transition(
        &self,
        buffer: &TitleBuffer,
        before: D3D12_RESOURCE_STATES,
        after: D3D12_RESOURCE_STATES,
    ) {
        let barrier_desc = Barrier {
            kind: 0,
            flags: 0,
            first: buffer.resource,
            second: u64::from(u32::MAX) | (u64::from(before.0 as u32) << 32),
            last: u64::from(after.0 as u32),
        };
        barrier(self.list, 1, &barrier_desc);
    }

    fn close(&self) {
        assert_eq!(close(self.list), S_OK);
    }

    fn reset(&self) {
        assert_eq!(reset(self.list, self.allocator, std::ptr::null()), S_OK);
    }
}

impl Drop for TitleList {
    fn drop(&mut self) {
        list_release(self.list);
        allocator_release(self.allocator);
    }
}

struct Shader {
    root: ID3D12RootSignature,
    pipeline: ID3D12PipelineState,
}

impl Shader {
    fn new(host: &Host, entry: PCSTR) -> Self {
        let mut code: Option<ID3DBlob> = None;
        let mut errors: Option<ID3DBlob> = None;
        // SAFETY: the source and entry name stay alive for compilation.
        unsafe {
            let result = D3DCompile(
                SOURCE.as_ptr().cast(),
                SOURCE.len(),
                s!("synthetic buffer copy"),
                None,
                None,
                entry,
                s!("cs_5_1"),
                0,
                0,
                &mut code,
                Some(&mut errors),
            );
            if let Err(error) = result {
                let diagnostics = errors.as_ref().map(|blob| {
                    String::from_utf8_lossy(std::slice::from_raw_parts(
                        blob.GetBufferPointer().cast::<u8>(),
                        blob.GetBufferSize(),
                    ))
                });
                panic!("shader compilation failed: {error}, {diagnostics:?}");
            }
            let code = code.unwrap();
            let bytes = std::slice::from_raw_parts(
                code.GetBufferPointer().cast::<u8>(),
                code.GetBufferSize(),
            );
            let root: ID3D12RootSignature = host.device.CreateRootSignature(0, bytes).unwrap();
            let pipeline = host
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
            Self { root, pipeline }
        }
    }

    fn write(&self, list: &TitleList, target: &TitleBuffer, word: usize, value: u32) {
        let list = list.recording();
        let shadow = target.shadow(&list, word, true);
        self.write_gpu(&list, target.gpu(&shadow, word), value);
    }

    fn write_gpu(&self, list: &HostList, address: u64, value: u32) {
        // SAFETY: the open list holds the shadow via the usual refresh/write steps.
        unsafe {
            list.list.SetComputeRootSignature(&self.root);
            list.list.SetPipelineState(&self.pipeline);
            list.list.SetComputeRoot32BitConstant(0, value, 0);
            list.list.SetComputeRootUnorderedAccessView(1, address);
            list.list.Dispatch(1, 1, 1);
        }
    }

    fn read(
        &self,
        list: &TitleList,
        source: &TitleBuffer,
        source_word: usize,
        target: &TitleBuffer,
    ) {
        let list = list.recording();
        let input = source.shadow(&list, source_word, false);
        self.read_gpu(&list, source.gpu(&input, source_word), target);
    }

    fn read_gpu(&self, list: &HostList, address: u64, target: &TitleBuffer) {
        let output = target.shadow(list, 0, true);
        // SAFETY: the open list holds both shadows via the usual refresh/write steps.
        unsafe {
            list.list.SetComputeRootSignature(&self.root);
            list.list.SetPipelineState(&self.pipeline);
            list.list.SetComputeRootShaderResourceView(0, address);
            list.list
                .SetComputeRootUnorderedAccessView(1, target.gpu(&output, 0));
            list.list.Dispatch(1, 1, 1);
        }
    }

    fn write_indirect(
        &self,
        list: &TitleList,
        args: &TitleBuffer,
        target: &TitleBuffer,
        value: u32,
    ) {
        let list = list.recording();
        let argument = D3D12_INDIRECT_ARGUMENT_DESC {
            Type: D3D12_INDIRECT_ARGUMENT_TYPE_DISPATCH,
            ..Default::default()
        };
        let desc = D3D12_COMMAND_SIGNATURE_DESC {
            ByteStride: 12,
            NumArgumentDescs: 1,
            pArgumentDescs: &argument,
            NodeMask: 0,
        };
        let mut signature: Option<ID3D12CommandSignature> = None;
        // SAFETY: valid one-argument signature without root arguments.
        unsafe {
            crate::host::host()
                .unwrap()
                .device
                .CreateCommandSignature(&desc, None::<&ID3D12RootSignature>, &mut signature)
                .unwrap();
        }
        let signature = signature.unwrap();
        list.keep(&signature);
        let input = shadow::find(args.address, 12).unwrap();
        input.refresh(&list, args.address, 12);
        input.gpu_writes(&list, args.address, 12);
        let output = target.shadow(&list, 0, true);
        assert!(crate::indirect::bound(
            &list,
            &input.buffer,
            input.gpu(args.address),
            (2, 12, 1),
        ));
        // SAFETY: the clamp performed the argument transition, and the
        // resources and signature are kept by the list.
        unsafe {
            list.list.SetComputeRootSignature(&self.root);
            list.list.SetPipelineState(&self.pipeline);
            list.list.SetComputeRoot32BitConstant(0, value, 0);
            list.list
                .SetComputeRootUnorderedAccessView(1, target.gpu(&output, 0));
            list.list.ExecuteIndirect(
                &signature,
                1,
                &input.buffer,
                input.offset(args.address),
                None,
                0,
            );
        }
        crate::indirect::done(&list, &input.buffer);
    }
}

struct Validation {
    queue: ID3D12InfoQueue1,
    cookie: u32,
    errors: Arc<Mutex<Vec<String>>>,
}

impl Validation {
    fn new(queue: ID3D12InfoQueue1) -> Self {
        let errors = Arc::new(Mutex::new(Vec::<String>::new()));
        let mut cookie = 0;
        // SAFETY: this instance keeps the callback context alive until unregistration.
        unsafe {
            queue.RegisterMessageCallback(
                Some(capture_debug_error),
                D3D12_MESSAGE_CALLBACK_IGNORE_FILTERS,
                Arc::as_ptr(&errors).cast_mut().cast(),
                &mut cookie,
            )
        }
        .unwrap();
        Self {
            queue,
            cookie,
            errors,
        }
    }

    fn assert_clean(&self) {
        let errors = self.errors.lock().unwrap();
        assert!(errors.is_empty(), "D3D12 debug errors: {errors:?}");
    }
}

impl Drop for Validation {
    fn drop(&mut self) {
        // SAFETY: the cookie was returned by this queue for our live context.
        unsafe { self.queue.UnregisterMessageCallback(self.cookie) }
            .expect("unregister D3D12 validation callback");
    }
}

unsafe extern "system" fn capture_debug_error(
    _: D3D12_MESSAGE_CATEGORY,
    severity: D3D12_MESSAGE_SEVERITY,
    id: D3D12_MESSAGE_ID,
    description: PCSTR,
    context: *mut std::ffi::c_void,
) {
    if severity.0 > D3D12_MESSAGE_SEVERITY_ERROR.0 {
        return;
    }
    // SAFETY: D3D12 supplies a message string and our registered context.
    let (errors, description) = unsafe {
        (
            &*context.cast::<Mutex<Vec<String>>>(),
            std::ffi::CStr::from_ptr(description.as_ptr().cast()).to_string_lossy(),
        )
    };
    errors
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .push(format!("{} {description}", id.0));
}

struct TestGpu {
    host: &'static Host,
    validation: Option<Validation>,
}

impl TestGpu {
    fn execute(&self, lists: &[&HostList]) {
        self.host.execute(lists);
        if let Some(validation) = &self.validation {
            validation.assert_clean();
        }
    }
}

fn test_gpu() -> Option<TestGpu> {
    std::env::var_os("VICTORY_TEST_BUFFER_COPY")?;
    let gbv = std::env::var_os("VICTORY_TEST_BUFFER_COPY_GBV").is_some();
    if gbv {
        static DEBUG: Once = Once::new();
        DEBUG.call_once(|| {
            assert!(
                std::env::var_os("VICTORY_D3D12_GBV").is_none(),
                "unset the production GBV opt-in to retain resource-state tracking"
            );
            let mut debug: Option<ID3D12Debug1> = None;
            // SAFETY: before the test host creates its first device.
            unsafe {
                D3D12GetDebugInterface(&mut debug).expect("D3D12 debug layer for GBV");
                let debug = debug.unwrap();
                debug.EnableDebugLayer();
                debug.SetEnableGPUBasedValidation(true);
            }
        });
    }
    let host = crate::host::host().expect("D3D12 adapter for buffer-copy regression");
    // SAFETY: DXGI queries with a live device.
    let desc = unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).unwrap();
        let adapter: IDXGIAdapter1 = factory
            .EnumAdapterByLuid(host.device.GetAdapterLuid())
            .unwrap();
        adapter.GetDesc1().unwrap()
    };
    let name = String::from_utf16_lossy(&desc.Description);
    let validation = host
        .device
        .cast::<ID3D12InfoQueue1>()
        .ok()
        .map(Validation::new);
    if gbv || std::env::var_os("VICTORY_D3D12_DEBUG").is_some() {
        assert!(
            validation.is_some(),
            "D3D12 debug layer was requested and is inactive"
        );
    }
    if gbv {
        let config = host.device.cast::<ID3D12DeviceConfiguration>().unwrap();
        // SAFETY: read-only query of the created device configuration.
        let config = unsafe { config.GetDesc() };
        assert_ne!(
            config.Flags.0 & D3D12_DEVICE_FLAG_GPU_BASED_VALIDATION_ENABLED.0,
            0
        );
        assert_eq!(
            config.GpuBasedValidationFlags
                & D3D12_GPU_BASED_VALIDATION_FLAGS_DISABLE_STATE_TRACKING.0 as u32,
            0,
        );
    }
    eprintln!(
        "buffer copy GPU test on {}, debug layer={}, full GBV={gbv}",
        name.trim_end_matches('\0'),
        validation.is_some(),
    );
    Some(TestGpu { host, validation })
}

#[test]
fn gpu_producer_copy_and_gpu_consumer_are_ordered() {
    let Some(gpu) = test_gpu() else { return };
    let writer = Shader::new(gpu.host, s!("write_word"));
    let reader = Shader::new(gpu.host, s!("read_word"));
    let a = TitleBuffer::new(&[13]);
    let b = TitleBuffer::new(&[0]);
    let output = TitleBuffer::new(&[0]);
    let list = TitleList::new();
    writer.write(&list, &a, 0, 500);
    list.transition(&a, UAV, COPY_OUT);
    list.copy(&b, 0, &a, 0, 4);
    list.transition(&b, COPY_IN, SRV);
    reader.read(&list, &b, 0, &output);
    list.close();
    gpu.execute(&[list.host()]);
    eprintln!(
        "title A={:?}, B={:?}, shader observed {:?}",
        a.words(),
        b.words(),
        output.words()
    );
    assert_eq!(a.words(), [500], "producer must execute");
    assert_eq!(
        output.words(),
        [500],
        "shader must see the GPU-produced value"
    );
    assert_eq!(b.words(), [500], "copy destination must reach title memory");
}

#[test]
fn gpu_read_before_copy_observes_original_value() {
    let Some(gpu) = test_gpu() else { return };
    let reader = Shader::new(gpu.host, s!("read_word"));
    let a = TitleBuffer::new(&[27]);
    let b = TitleBuffer::new(&[13]);
    let before = TitleBuffer::new(&[0]);
    let after = TitleBuffer::new(&[0]);
    let list = TitleList::new();
    reader.read(&list, &b, 0, &before);
    list.transition(&b, SRV, COPY_IN);
    list.copy(&b, 0, &a, 0, 4);
    list.transition(&b, COPY_IN, SRV);
    reader.read(&list, &b, 0, &after);
    list.close();
    gpu.execute(&[list.host()]);
    eprintln!(
        "shader before copy {:?}, after copy {:?}",
        before.words(),
        after.words()
    );
    assert_eq!(before.words(), [13]);
    assert_eq!(after.words(), [27]);
}

#[test]
fn consecutive_copies_forward_gpu_produced_bytes() {
    let Some(gpu) = test_gpu() else { return };
    let writer = Shader::new(gpu.host, s!("write_word"));
    let a = TitleBuffer::new(&[13]);
    let b = TitleBuffer::new(&[0]);
    let c = TitleBuffer::new(&[0]);
    let list = TitleList::new();
    writer.write(&list, &a, 0, 500);
    list.transition(&a, UAV, COPY_OUT);
    list.copy(&b, 0, &a, 0, 4);
    list.transition(&b, COPY_IN, COPY_OUT);
    list.copy(&c, 0, &b, 0, 4);
    list.close();
    gpu.execute(&[list.host()]);
    assert_eq!(b.words(), [500]);
    assert_eq!(c.words(), [500]);
}

#[test]
fn ordered_copies_span_multiple_lists_in_one_submission() {
    let Some(gpu) = test_gpu() else { return };
    let writer = Shader::new(gpu.host, s!("write_word"));
    let reader = Shader::new(gpu.host, s!("read_word"));
    let a = TitleBuffer::new(&[13]);
    let b = TitleBuffer::new(&[0]);
    let output = TitleBuffer::new(&[0]);
    let produce = TitleList::new();
    let copy = TitleList::new();
    let consume = TitleList::new();
    writer.write(&produce, &a, 0, 500);
    copy.transition(&a, UAV, COPY_OUT);
    copy.copy(&b, 0, &a, 0, 4);
    copy.transition(&b, COPY_IN, SRV);
    reader.read(&consume, &b, 0, &output);
    for list in [&produce, &copy, &consume] {
        list.close();
    }
    gpu.execute(&[produce.host(), copy.host(), consume.host()]);
    assert_eq!(output.words(), [500]);
    assert_eq!(b.words(), [500]);
}

#[test]
fn copy_reads_write_from_an_earlier_submission() {
    let Some(gpu) = test_gpu() else { return };
    let writer = Shader::new(gpu.host, s!("write_word"));
    let reader = Shader::new(gpu.host, s!("read_word"));
    let a = TitleBuffer::new(&[13]);
    let b = TitleBuffer::new(&[0]);
    let output = TitleBuffer::new(&[0]);
    let produce = TitleList::new();
    let copy = TitleList::new();
    writer.write(&produce, &a, 0, 500);
    copy.copy(&b, 0, &a, 0, 4);
    copy.transition(&b, COPY_IN, SRV);
    reader.read(&copy, &b, 0, &output);
    produce.close();
    copy.close();
    gpu.execute(&[produce.host()]);
    gpu.execute(&[copy.host()]);
    assert_eq!(output.words(), [500]);
    assert_eq!(b.words(), [500]);
}

#[test]
fn buffer_copy_resource_preserves_order() {
    let Some(gpu) = test_gpu() else { return };
    let writer = Shader::new(gpu.host, s!("write_word"));
    let reader = Shader::new(gpu.host, s!("read_word"));
    let a = TitleBuffer::new(&[13, 17]);
    let b = TitleBuffer::new(&[0, 0]);
    let output = TitleBuffer::new(&[0]);
    let list = TitleList::new();
    writer.write(&list, &a, 0, 500);
    list.transition(&a, UAV, COPY_OUT);
    list.copy_resource(&b, &a);
    list.transition(&b, COPY_IN, SRV);
    reader.read(&list, &b, 0, &output);
    list.close();
    gpu.execute(&[list.host()]);
    assert_eq!(output.words(), [500]);
    assert_eq!(b.words(), [500, 17]);
}

#[test]
fn partial_copy_accepts_exact_end_and_rejects_invalid_ranges() {
    let Some(gpu) = test_gpu() else { return };
    let a = TitleBuffer::new(&[1, 2, 3, 4]);
    let b = TitleBuffer::new(&[17, 18, 19, 20]);
    let list = TitleList::new();
    list.copy(&b, 8, &a, 8, 8);
    list.copy(&b, 0, &a, 12, 8);
    list.copy(&b, 12, &a, 0, 8);
    list.copy(&b, 0, &a, u64::MAX, 4);
    list.copy(&b, u64::MAX, &a, 0, 4);
    list.copy(&b, 16, &a, 16, 0);
    copy_buffer_region(list.list, b.resource, 0, std::ptr::null(), 0, 4);
    copy_buffer_region(list.list, std::ptr::null(), 0, a.resource, 0, 4);
    list.close();
    gpu.execute(&[list.host()]);
    assert_eq!(b.words(), [17, 18, 3, 4]);
}

#[test]
fn repeated_copy_observes_latest_cpu_data() {
    let Some(gpu) = test_gpu() else { return };
    let reader = Shader::new(gpu.host, s!("read_word"));
    let a = TitleBuffer::new(&[13]);
    let b = TitleBuffer::new(&[0]);
    let output = TitleBuffer::new(&[0]);
    let list = TitleList::new();
    list.copy(&b, 0, &a, 0, 4);
    list.transition(&b, COPY_IN, SRV);
    reader.read(&list, &b, 0, &output);
    list.close();
    for value in [19, 500, 41] {
        a.set(0, value);
        gpu.execute(&[list.host()]);
        assert_eq!(output.words(), [value]);
        assert_eq!(b.words(), [value]);
    }
}

#[test]
fn cpu_staging_fill_observes_copy_destination() {
    let Some(gpu) = test_gpu() else { return };
    let source = TitleBuffer::new(&[27]);
    let dest = TitleBuffer::new(&[13]);
    let list = TitleList::new();
    let (_upload, mapped) = list.host().upload(gpu.host, 4).unwrap();
    list.copy(&dest, 0, &source, 0, 4);
    list.recording()
        .defer_fill(mapped, dest.address as *const u8, 4);
    list.close();
    gpu.execute(&[list.host()]);
    // SAFETY: the completed submission filled four bytes of the live upload mapping.
    assert_eq!(unsafe { mapped.cast::<u32>().read_unaligned() }, 27);
    assert_eq!(dest.words(), [27]);
}

#[test]
fn cpu_staging_reads_back_a_gpu_ordered_copy_before_gpu_consumes_upload() {
    let Some(gpu) = test_gpu() else { return };
    let writer = Shader::new(gpu.host, s!("write_word"));
    let reader = Shader::new(gpu.host, s!("read_word"));
    let source = TitleBuffer::new(&[27]);
    let dest = TitleBuffer::new(&[13]);
    let output = TitleBuffer::new(&[0]);
    let list = TitleList::new();
    let (upload, mapped) = list.host().upload(gpu.host, 4).unwrap();
    writer.write(&list, &source, 0, 500);
    list.copy(&dest, 0, &source, 0, 4);
    let recording = list.recording();
    recording.defer_fill(mapped, dest.address as *const u8, 4);
    // SAFETY: the upload is kept by the list and contains four bytes.
    reader.read_gpu(
        &recording,
        unsafe { upload.GetGPUVirtualAddress() },
        &output,
    );
    list.close();
    gpu.execute(&[list.host()]);
    assert_eq!(dest.words(), [500]);
    assert_eq!(output.words(), [500]);
}

#[test]
fn disjoint_ranges_in_one_native_buffer_copy_in_gpu_order() {
    let Some(gpu) = test_gpu() else { return };
    let writer = Shader::new(gpu.host, s!("write_word"));
    let reader = Shader::new(gpu.host, s!("read_word"));
    let buffer = TitleBuffer::new(&[11, 22, 33]);
    let output = TitleBuffer::new(&[0]);
    let list = TitleList::new();
    writer.write(&list, &buffer, 0, 500);
    list.copy(&buffer, 4, &buffer, 0, 4);
    list.copy(&buffer, 0, &buffer, 0, 4);
    list.copy(&buffer, 4, &buffer, 0, 8);
    reader.read(&list, &buffer, 1, &output);
    list.close();
    gpu.execute(&[list.host()]);
    assert_eq!(buffer.words(), [500, 500, 33]);
    assert_eq!(output.words(), [500]);
}

#[test]
fn independent_copies_share_a_handoff_and_duplicate_copy_preserves_result() {
    let Some(gpu) = test_gpu() else { return };
    let writer = Shader::new(gpu.host, s!("write_word"));
    let reader = Shader::new(gpu.host, s!("read_word"));
    let a = TitleBuffer::new(&[13]);
    let b = TitleBuffer::new(&[27]);
    let dest = TitleBuffer::new(&[0, 0]);
    let output = TitleBuffer::new(&[0]);
    let list = TitleList::new();
    writer.write(&list, &a, 0, 500);
    list.copy(&dest, 0, &a, 0, 4);
    list.copy(&dest, 0, &a, 0, 4);
    list.copy(&dest, 4, &b, 0, 4);
    reader.read(&list, &dest, 1, &output);
    list.close();
    gpu.execute(&[list.host()]);
    assert_eq!(dest.words(), [500, 27]);
    assert_eq!(output.words(), [27]);
}

#[test]
fn ordered_copy_keeps_released_buffer_backing_until_reset() {
    let Some(gpu) = test_gpu() else { return };
    let writer = Shader::new(gpu.host, s!("write_word"));
    let reader = Shader::new(gpu.host, s!("read_word"));
    let source = TitleBuffer::new(&[27]);
    let dest = TitleBuffer::new(&[13]);
    let output = TitleBuffer::new(&[0]);
    let list = TitleList::new();
    writer.write(&list, &source, 0, 500);
    list.copy(&dest, 0, &source, 0, 4);
    reader.read(&list, &dest, 0, &output);
    list.close();
    drop(source);
    drop(dest);
    gpu.execute(&[list.host()]);
    assert_eq!(output.words(), [500]);
    list.reset();
    let cpu_source = TitleBuffer::new(&[77]);
    list.copy(&output, 0, &cpu_source, 0, 4);
    list.close();
    gpu.execute(&[list.host()]);
    assert_eq!(output.words(), [77]);
}

#[test]
fn copyback_reconciles_a_later_destination_in_a_snapshot_gap() {
    let Some(gpu) = test_gpu() else { return };
    let writer = Shader::new(gpu.host, s!("write_word"));
    let source = TitleBuffer::new(&[0]);
    let dest = TitleBuffer::new(&[13, 27, 59]);
    let list = TitleList::new();
    writer.write(&list, &dest, 0, 500);
    list.copy(&dest, 8, &source, 0, 4);
    list.copy(&dest, 4, &source, 0, 4);
    list.close();
    gpu.execute(&[list.host()]);
    assert_eq!(dest.words(), [500, 0, 0]);
}

#[test]
fn copyback_preserves_order_for_distinct_shadows_over_same_title_word() {
    let Some(gpu) = test_gpu() else { return };
    let writer = Shader::new(gpu.host, s!("write_word"));
    let title = TitleBuffer::new(&[13, 27]);
    let word = title.address + 4;
    // Model an inner buffer registration in the same title allocation.
    let key = std::ptr::from_ref(&word) as usize;
    shadow::register(key, word, 4);
    let inner = shadow::find(word, 4);
    shadow::unregister(key, word);
    let inner = inner.unwrap();
    let outer = shadow::find(title.address, 8).unwrap();
    assert!(!Arc::ptr_eq(&inner, &outer));
    let (first, second) = if Arc::as_ptr(&inner) > Arc::as_ptr(&outer) {
        (inner, outer)
    } else {
        (outer, inner)
    };

    let list = TitleList::new();
    let recording = list.recording();
    for (shadow, value) in [(first, 111), (second, 222)] {
        shadow.refresh(&recording, word, 4);
        shadow.gpu_writes(&recording, word, 4);
        writer.write_gpu(&recording, shadow.gpu(word), value);
    }
    list.close();
    gpu.execute(&[list.host()]);
    assert_eq!(title.words(), [13, 222]);
}

#[test]
fn copy_handoffs_between_indirect_producer_and_consumer() {
    let Some(gpu) = test_gpu() else { return };
    let writer = Shader::new(gpu.host, s!("write_word"));
    let a = TitleBuffer::new(&[1, 1, 1]);
    let b = TitleBuffer::new(&[0, 0, 0]);
    let before = TitleBuffer::new(&[0]);
    let after = TitleBuffer::new(&[0]);
    let list = TitleList::new();
    writer.write_indirect(&list, &a, &before, 55);
    list.copy(&b, 0, &a, 0, 12);
    writer.write_indirect(&list, &b, &after, 77);
    list.close();
    for _ in 0..2 {
        before.set(0, 0);
        after.set(0, 0);
        gpu.execute(&[list.host()]);
        assert_eq!(before.words(), [55]);
        assert_eq!(after.words(), [77]);
        assert_eq!(b.words(), [1, 1, 1]);
    }
}

#[test]
fn native_copy_control_with_explicit_known_states() {
    let Some(gpu) = test_gpu() else { return };
    let writer = Shader::new(gpu.host, s!("write_word"));
    let reader = Shader::new(gpu.host, s!("read_word"));
    let a = TitleBuffer::new(&[13]);
    let b = TitleBuffer::new(&[0]);
    let output = TitleBuffer::new(&[0]);
    let list = TitleList::new();
    writer.write(&list, &a, 0, 500);
    let source = shadow::find(a.address, 4).unwrap();
    let target = b.shadow(list.host(), 0, true);
    let native = &list.host().list;
    // SAFETY: this reference case supplies the known native states itself.
    // The title-facing copy is exercised by the other tests in this module.
    unsafe {
        native.ResourceBarrier(&[
            crate::host::transition(&source.buffer, UAV, COPY_OUT),
            crate::host::transition(&target.buffer, D3D12_RESOURCE_STATE_COMMON, COPY_IN),
        ]);
        native.CopyBufferRegion(&target.buffer, 0, &source.buffer, 0, 4);
        native.ResourceBarrier(&[crate::host::transition(&target.buffer, COPY_IN, SRV)]);
    }
    reader.read(&list, &b, 0, &output);
    list.close();
    gpu.execute(&[list.host()]);
    assert_eq!(a.words(), [500]);
    assert_eq!(b.words(), [500]);
    assert_eq!(output.words(), [500]);
}
