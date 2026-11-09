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

use super::{Adapter, Fallback, Gpu, Nodes, Part, Placed, runs_fully_on_gpu};

/// Name of ONNX Runtime's CPU execution provider in the placement info.
const CPU_PROVIDER: &str = "CPUExecutionProvider";

// The `repr(C)` structs below, and the `ort::sys` ones read in `placement`, match the Dawn and ONNX Runtime 1.31.0 that the
// pinned `ort =2.0.0-rc.15` downloads; changing that pin means checking each against its headers.

/// Dawn's `WGPU_STRLEN`: a string view of this length is NUL-terminated instead.
const WGPU_STRLEN: usize = usize::MAX;

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
    let bytes = if view.length == WGPU_STRLEN {
        // SAFETY: Dawn fills a view of `WGPU_STRLEN` with a NUL-terminated string it owns until
        // `wgpuAdapterInfoFreeMembers`.
        unsafe { CStr::from_ptr(view.data) }.to_bytes()
    } else {
        // SAFETY: Dawn fills `view` with `length` bytes it owns until `wgpuAdapterInfoFreeMembers`.
        unsafe { std::slice::from_raw_parts(view.data.cast::<u8>(), view.length) }
    };
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

/// Opens `path`, one `part` of the model, on the GPU, and only if ONNX Runtime put all of it there that matters.
///
/// # Errors
/// [`Fallback::DriverFailed`] when the session cannot start on the GPU, [`Fallback::CannotRunFully`] when it starts
/// but leaves compute on the CPU.
pub fn open(
    builder: SessionBuilder,
    path: &Path,
    part: Part,
) -> Result<(Session, Nodes), Fallback> {
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
    let nodes = Nodes::of(&placed);
    if runs_fully_on_gpu(part, &placed) {
        Ok((session, nodes))
    } else {
        tracing::warn!(
            "The {part:?} {} has {} of {} nodes on the GPU, which is not enough",
            path.display(),
            nodes.on_gpu,
            nodes.total
        );
        Err(Fallback::CannotRunFully)
    }
}

/// Whether an ONNX Runtime call succeeded; a failure's status is released.
fn succeeded(status: ort::sys::OrtStatusPtr) -> bool {
    if status.0.is_null() {
        return true;
    }
    // SAFETY: `status` is a non-null status that ONNX Runtime returned and nothing else holds; it is released once.
    unsafe { (ort::api().ReleaseStatus)(status.0) };
    false
}

/// Every node of the session with the operator it runs and whether the CPU provider took it.
fn placement(session: &Session) -> Option<Vec<Placed>> {
    let api = ort::api();
    let mut placed = Vec::new();
    let mut subgraphs: *const *const ort::sys::OrtEpAssignedSubgraph = std::ptr::null();
    let mut count = 0_usize;
    // SAFETY: `session` is live; on success the returned arrays belong to it and stay valid while it does, which
    // is longer than this function, every status is checked, and every pointer read is checked for null.
    unsafe {
        if !succeeded((api.Session_GetEpGraphAssignmentInfo)(
            session.ptr(),
            &raw mut subgraphs,
            &raw mut count,
        )) {
            return None;
        }
        for index in 0..count {
            let subgraph = *subgraphs.add(index);
            let mut provider: *const c_char = std::ptr::null();
            let mut nodes: *const *const ort::sys::OrtEpAssignedNode = std::ptr::null();
            let mut length = 0_usize;
            if !succeeded((api.EpAssignedSubgraph_GetEpName)(
                subgraph,
                &raw mut provider,
            )) || !succeeded((api.EpAssignedSubgraph_GetNodes)(
                subgraph,
                &raw mut nodes,
                &raw mut length,
            )) || provider.is_null()
            {
                return None;
            }
            let on_cpu = CStr::from_ptr(provider).to_bytes() == CPU_PROVIDER.as_bytes();
            for node in 0..length {
                let mut operator: *const c_char = std::ptr::null();
                if !succeeded((api.EpAssignedNode_GetOperatorType)(
                    *nodes.add(node),
                    &raw mut operator,
                )) || operator.is_null()
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

#[cfg(test)]
mod tests {
    use std::ffi::CString;

    use super::*;

    #[test]
    fn dawn_strings_are_read_by_their_length_or_up_to_the_nul_when_the_length_is_the_sentinel() {
        let name = CString::new("NVIDIA GeForce RTX 5070 Ti").unwrap();
        for (case, view, expected) in [
            (
                "by length",
                StringView {
                    data: name.as_ptr(),
                    length: 6,
                },
                "NVIDIA",
            ),
            (
                "up to the NUL",
                StringView {
                    data: name.as_ptr(),
                    length: WGPU_STRLEN,
                },
                "NVIDIA GeForce RTX 5070 Ti",
            ),
            (
                "no data",
                StringView {
                    data: std::ptr::null(),
                    length: WGPU_STRLEN,
                },
                "",
            ),
        ] {
            assert_eq!(text(&view), expected, "{case}");
        }
    }

    #[test]
    fn an_onnx_runtime_call_succeeded_only_when_it_returned_no_status() {
        let message = CString::new("it failed").unwrap();
        // SAFETY: `message` is a NUL-terminated string that outlives the call, which copies it.
        let failure = unsafe {
            (ort::api().CreateStatus)(ort::sys::OrtErrorCode::ORT_FAIL, message.as_ptr())
        };
        assert!(!succeeded(failure));
        assert!(succeeded(ort::sys::OrtStatusPtr::default()));
    }
}
