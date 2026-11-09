//! The Linux GPU backend: Dawn's adapter check, a WebGPU session, and the read of where ONNX Runtime placed its nodes.

#![expect(
    unsafe_code,
    reason = "Dawn's C API and ONNX Runtime's placement info are raw FFI; each block says why it is sound"
)]

use std::ffi::{CStr, c_char, c_void};
use std::path::Path;

use ort::AsPointer;
use ort::ep::WebGPU;
use ort::session::Session;
use ort::session::builder::SessionBuilder;

use super::{Adapter, Fallback, Gpu, Placed, runs_fully_on_gpu};

/// Name of ONNX Runtime's CPU execution provider in the placement info.
const CPU_PROVIDER: &str = "CPUExecutionProvider";

#[repr(C)]
struct StringView {
    data: *const c_char,
    length: usize,
}

#[repr(C)]
struct RequestOptions {
    next: *const c_void,
    feature_level: u32,
    power_preference: u32,
    force_fallback: u32,
    backend_type: u32,
    surface: *const c_void,
}

type RequestCallback = extern "C" fn(u32, *mut c_void, StringView, *mut c_void, *mut c_void);

#[repr(C)]
struct CallbackInfo {
    next: *const c_void,
    mode: u32,
    callback: RequestCallback,
    user_data1: *mut c_void,
    user_data2: *mut c_void,
}

#[repr(C)]
struct AdapterInfo {
    next: *mut c_void,
    vendor: StringView,
    architecture: StringView,
    device: StringView,
    description: StringView,
    backend_type: u32,
    adapter_type: u32,
    vendor_id: u32,
    device_id: u32,
    subgroup_min_size: u32,
    subgroup_max_size: u32,
}

#[repr(C)]
struct Future {
    id: u64,
}

#[repr(C)]
struct FutureWaitInfo {
    future: Future,
    completed: u32,
}

// Linked through ONNX Runtime's own dependency on `libwebgpu_dawn.so`.
unsafe extern "C" {
    fn wgpuCreateInstance(descriptor: *const c_void) -> *mut c_void;
    fn wgpuInstanceRelease(instance: *mut c_void);
    fn wgpuInstanceRequestAdapter(
        instance: *mut c_void,
        options: *const RequestOptions,
        callback: CallbackInfo,
    ) -> Future;
    fn wgpuInstanceWaitAny(
        instance: *mut c_void,
        count: usize,
        infos: *mut FutureWaitInfo,
        timeout_ns: u64,
    ) -> u32;
    fn wgpuAdapterGetInfo(adapter: *mut c_void, info: *mut AdapterInfo) -> u32;
    fn wgpuAdapterInfoFreeMembers(info: AdapterInfo);
    fn wgpuAdapterRelease(adapter: *mut c_void);
}

const REQUEST_SUCCESS: u32 = 1;
const CALLBACK_WAIT_ANY_ONLY: u32 = 1;
const FEATURE_LEVEL_CORE: u32 = 2;
const ADAPTER_DISCRETE: u32 = 1;

/// Dawn's `WGPUBackendType` names, for the backends a desktop GPU can use.
fn backend_name(backend: u32) -> &'static str {
    match backend {
        3 => "Metal",
        4 => "D3D12",
        5 => "OpenGL ES",
        6 => "Vulkan",
        7 => "OpenGL",
        _ => "unknown API",
    }
}

#[repr(C)]
struct Reply {
    status: u32,
    adapter: *mut c_void,
}

extern "C" fn on_adapter(
    status: u32,
    adapter: *mut c_void,
    _message: StringView,
    reply: *mut c_void,
    _unused: *mut c_void,
) {
    // SAFETY: `reply` is the `Reply` that `adapter` passed to Dawn, alive until `wgpuInstanceWaitAny` returns, which
    // is the only place the callback runs (`WaitAnyOnly`).
    let reply = unsafe { &mut *reply.cast::<Reply>() };
    reply.status = status;
    reply.adapter = adapter;
}

fn text(view: &StringView) -> String {
    if view.data.is_null() {
        return String::new();
    }
    // SAFETY: Dawn fills `view` with `length` bytes it owns until `wgpuAdapterInfoFreeMembers`.
    let bytes = unsafe { std::slice::from_raw_parts(view.data.cast::<u8>(), view.length) };
    String::from_utf8_lossy(bytes).into_owned()
}

/// What Dawn's default adapter is. Only a discrete one counts: ONNX Runtime runs happily on a software
/// rasteriser, and integrated GPUs were slower than the CPU for every model measured.
pub fn adapter() -> Adapter {
    // SAFETY: a null descriptor asks for the default instance; the instance is released before returning.
    let instance = unsafe { wgpuCreateInstance(std::ptr::null()) };
    if instance.is_null() {
        return Adapter::Unusable;
    }
    let mut reply = Reply {
        status: 0,
        adapter: std::ptr::null_mut(),
    };
    let options = RequestOptions {
        next: std::ptr::null(),
        feature_level: FEATURE_LEVEL_CORE,
        power_preference: 0,
        force_fallback: 0,
        backend_type: 0,
        surface: std::ptr::null(),
    };
    let callback = CallbackInfo {
        next: std::ptr::null(),
        mode: CALLBACK_WAIT_ANY_ONLY,
        callback: on_adapter,
        user_data1: (&raw mut reply).cast(),
        user_data2: std::ptr::null_mut(),
    };
    // SAFETY: `instance` is live, `options` and `callback` outlive the calls, and `reply` outlives the wait that
    // runs the callback; a zero timeout only polls, so the wait cannot block on a driver.
    let found = unsafe {
        let mut wait = FutureWaitInfo {
            future: wgpuInstanceRequestAdapter(instance, &raw const options, callback),
            completed: 0,
        };
        wgpuInstanceWaitAny(instance, 1, &raw mut wait, 0);
        wait.completed != 0 && reply.status == REQUEST_SUCCESS && !reply.adapter.is_null()
    };
    let result = if found {
        describe(reply.adapter)
    } else {
        Adapter::Unusable
    };
    if !reply.adapter.is_null() {
        // SAFETY: the adapter came from this instance and is not used after this.
        unsafe { wgpuAdapterRelease(reply.adapter) };
    }
    // SAFETY: nothing created from the instance is alive.
    unsafe { wgpuInstanceRelease(instance) };
    result
}

fn describe(adapter: *mut c_void) -> Adapter {
    let mut info = AdapterInfo {
        next: std::ptr::null_mut(),
        vendor: StringView {
            data: std::ptr::null(),
            length: 0,
        },
        architecture: StringView {
            data: std::ptr::null(),
            length: 0,
        },
        device: StringView {
            data: std::ptr::null(),
            length: 0,
        },
        description: StringView {
            data: std::ptr::null(),
            length: 0,
        },
        backend_type: 0,
        adapter_type: 0,
        vendor_id: 0,
        device_id: 0,
        subgroup_min_size: 0,
        subgroup_max_size: 0,
    };
    // SAFETY: `adapter` is a live adapter and `info` is a zeroed `WGPUAdapterInfo` of Dawn's layout.
    let status = unsafe { wgpuAdapterGetInfo(adapter, &raw mut info) };
    if status != REQUEST_SUCCESS {
        return Adapter::Unusable;
    }
    let found = if info.adapter_type == ADAPTER_DISCRETE {
        Adapter::Discrete(Gpu {
            name: text(&info.device),
            api: backend_name(info.backend_type),
        })
    } else {
        Adapter::Unusable
    };
    // SAFETY: `info` was filled by `wgpuAdapterGetInfo` and is not read after this.
    unsafe { wgpuAdapterInfoFreeMembers(info) };
    found
}

/// Opens `path` on the GPU, and only if ONNX Runtime put the whole model there.
///
/// # Errors
/// [`Fallback::DriverFailed`] when the session cannot start on the GPU, [`Fallback::CannotRunFully`] when it starts
/// but leaves compute on the CPU.
pub fn open(builder: SessionBuilder, path: &Path) -> Result<Session, Fallback> {
    let driver = |error: &dyn std::fmt::Display| {
        tracing::warn!(
            "The GPU session for {} did not start: {error}",
            path.display()
        );
        Fallback::DriverFailed
    };
    let start = || -> ort::Result<Session> {
        builder
            .with_config_entry("session.record_ep_graph_assignment_info", "1")?
            .with_execution_providers([WebGPU::default().build().error_on_failure()])?
            .commit_from_file(path)
    };
    let session = start().map_err(|error| driver(&error))?;
    let placed = placement(&session)
        .ok_or_else(|| driver(&"ONNX Runtime did not report where it placed the model"))?;
    if runs_fully_on_gpu(&placed) {
        Ok(session)
    } else {
        tracing::warn!("{} is not fully placed on the GPU", path.display());
        Err(Fallback::CannotRunFully)
    }
}

/// Every node of the session with the operator it runs and whether the CPU provider took it.
fn placement(session: &Session) -> Option<Vec<Placed>> {
    let api = ort::api();
    let mut placed = Vec::new();
    let mut subgraphs: *const *const ort::sys::OrtEpAssignedSubgraph = std::ptr::null();
    let mut count = 0_usize;
    // SAFETY: `session` is live; on success the returned arrays belong to it and stay valid while it does, which
    // is longer than this function, and every status is checked.
    unsafe {
        if !(api.Session_GetEpGraphAssignmentInfo)(
            session.ptr(),
            &raw mut subgraphs,
            &raw mut count,
        )
        .0
        .is_null()
        {
            return None;
        }
        for index in 0..count {
            let subgraph = *subgraphs.add(index);
            let mut provider: *const c_char = std::ptr::null();
            let mut nodes: *const *const ort::sys::OrtEpAssignedNode = std::ptr::null();
            let mut length = 0_usize;
            if !(api.EpAssignedSubgraph_GetEpName)(subgraph, &raw mut provider)
                .0
                .is_null()
                || !(api.EpAssignedSubgraph_GetNodes)(subgraph, &raw mut nodes, &raw mut length)
                    .0
                    .is_null()
            {
                return None;
            }
            let on_cpu = CStr::from_ptr(provider).to_bytes() == CPU_PROVIDER.as_bytes();
            for node in 0..length {
                let mut operator: *const c_char = std::ptr::null();
                if !(api.EpAssignedNode_GetOperatorType)(*nodes.add(node), &raw mut operator)
                    .0
                    .is_null()
                {
                    return None;
                }
                placed.push(Placed {
                    operator: CStr::from_ptr(operator).to_string_lossy().into_owned(),
                    on_cpu,
                });
            }
        }
    }
    Some(placed)
}
