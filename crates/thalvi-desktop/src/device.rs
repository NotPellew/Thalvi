//! Owns which hardware on-device recognition runs on: the adapter check, the load plan, and the reasons for the CPU.

use std::future::Future;
use std::sync::{Mutex, PoisonError};

use serde_json::{Value, json};

#[cfg(not(target_os = "linux"))]
mod cpu_only;
#[cfg(not(target_os = "linux"))]
use cpu_only as native;
#[cfg(target_os = "linux")]
mod webgpu;
#[cfg(target_os = "linux")]
use webgpu as native;

pub(crate) use native::open;

/// A discrete graphics adapter that can run recognition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gpu {
    pub name: String,
    /// The graphics API, such as Vulkan.
    pub api: &'static str,
}

/// What the check of the default graphics adapter found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Adapter {
    Discrete(Gpu),
    /// No adapter, or only an integrated or software one.
    Unusable,
    /// This OS runs recognition on the CPU only.
    Unsupported,
}

/// Why recognition runs on the CPU. Each is shown to the user next to the model.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fallback {
    NoUsableGpu,
    /// A discrete adapter was found, but a session on it did not start.
    DriverFailed,
    /// The session started but ONNX Runtime left compute on the CPU.
    CannotRunFully,
    /// Only Nemotron Unified is faster on a GPU.
    CpuModel,
    WindowsUnsupported,
}

impl Fallback {
    /// The plain reason, completing "Runs on the CPU: …".
    #[must_use]
    pub fn reason(self) -> &'static str {
        match self {
            Self::NoUsableGpu => "no usable GPU",
            Self::DriverFailed => "the GPU driver failed to start",
            Self::CannotRunFully => "the model can't run fully on the GPU",
            Self::CpuModel => "this model runs on the CPU",
            Self::WindowsUnsupported => "GPU recognition isn't supported on Windows yet",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::NoUsableGpu => "no_usable_gpu",
            Self::DriverFailed => "driver_failed",
            Self::CannotRunFully => "cannot_run_fully",
            Self::CpuModel => "cpu_model",
            Self::WindowsUnsupported => "windows_unsupported",
        }
    }
}

/// Where a loaded model runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Device {
    Gpu(Gpu),
    Cpu(Fallback),
}

impl Device {
    /// Where a model will be loaded first: on the GPU if it uses one and the adapter check finds a discrete one.
    /// The adapter is checked only for a model that uses a GPU.
    #[must_use]
    pub fn plan(model_uses_gpu: bool, adapter: impl FnOnce() -> Adapter) -> Self {
        if !model_uses_gpu {
            return Self::Cpu(Fallback::CpuModel);
        }
        match adapter() {
            Adapter::Discrete(gpu) => Self::Gpu(gpu),
            Adapter::Unusable => Self::Cpu(Fallback::NoUsableGpu),
            Adapter::Unsupported => Self::Cpu(Fallback::WindowsUnsupported),
        }
    }

    /// The Settings line: what is running now (`loaded`) or what loading will do.
    #[must_use]
    pub fn line(&self, loaded: bool) -> String {
        let verb = if loaded { "Runs" } else { "Will run" };
        match self {
            Self::Gpu(gpu) => format!("{verb} on the GPU ({}, {})", gpu.name, gpu.api),
            Self::Cpu(reason) => format!("{verb} on the CPU: {}", reason.reason()),
        }
    }

    /// `fields` of a speech metric with the device and any fallback reason added.
    #[must_use]
    pub fn tag(&self, mut fields: Value) -> Value {
        let device = match self {
            Self::Gpu(gpu) => json!({"device": "gpu", "gpu": gpu.name, "gpu_api": gpu.api}),
            Self::Cpu(reason) => json!({"device": "cpu", "fallback": reason.key()}),
        };
        if let (Some(fields), Value::Object(device)) = (fields.as_object_mut(), device) {
            fields.extend(device);
        }
        fields
    }
}

/// The default adapter of this computer, checked now.
#[must_use]
pub fn adapter() -> Adapter {
    native::adapter()
}

/// How an attempt to load on the GPU failed.
#[derive(Debug, PartialEq, Eq)]
pub enum Attempt<E> {
    /// The CPU takes over, with this reason.
    Fallback(Fallback),
    /// Not a GPU problem; the load fails as it would on the CPU.
    Failed(E),
}

/// Loads on the GPU when the plan says so and falls back to the CPU, with the reason, when that fails.
///
/// # Errors
/// The error of a load that is not about the GPU, or of the CPU load.
pub async fn load<M, E, G, C>(
    plan: Device,
    gpu: impl FnOnce(Gpu) -> G,
    cpu: impl FnOnce(Fallback) -> C,
) -> Result<M, E>
where
    G: Future<Output = Result<M, Attempt<E>>>,
    C: Future<Output = Result<M, E>>,
{
    let reason = match plan {
        Device::Gpu(found) => match gpu(found).await {
            Ok(model) => return Ok(model),
            Err(Attempt::Fallback(reason)) => reason,
            Err(Attempt::Failed(error)) => return Err(error),
        },
        Device::Cpu(reason) => reason,
    };
    cpu(reason).await
}

/// One node of a loaded session: its operator, and whether the CPU provider runs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placed {
    pub operator: String,
    pub on_cpu: bool,
}

/// Fewest nodes, in percent, that must run on the GPU.
const MOST_ON_GPU: usize = 98;

/// ONNX Runtime keeps shape arithmetic on the CPU, so "everything on the GPU" fails for every export. What
/// matters is that no operator that does the arithmetic of the model is among them, and that almost all nodes are not.
#[must_use]
pub fn runs_fully_on_gpu(nodes: &[Placed]) -> bool {
    let compute = |operator: &str| {
        matches!(
            operator,
            "MatMul" | "Gemm" | "Conv" | "LSTM" | "LayerNormalization" | "Softmax"
        ) || operator.ends_with("Integer")
            || operator.starts_with("DynamicQuantize")
    };
    let on_cpu = nodes.iter().filter(|node| node.on_cpu).count();
    !nodes
        .iter()
        .any(|node| node.on_cpu && compute(&node.operator))
        && (nodes.len() - on_cpu) * 100 >= nodes.len() * MOST_ON_GPU
}

/// The model the last load put on a device, for Settings to show.
static LOADED: Mutex<Option<(String, Device)>> = Mutex::new(None);

/// Remembers where the model called `label` runs.
pub fn loaded(label: &str, device: &Device) {
    *LOADED.lock().unwrap_or_else(PoisonError::into_inner) =
        Some((label.to_owned(), device.clone()));
}

/// The device the model called `label` was last loaded on, if it was loaded in this run.
#[must_use]
pub fn loaded_on(label: &str) -> Option<Device> {
    LOADED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .filter(|(name, _)| name == label)
        .map(|(_, device)| device.clone())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    fn rtx() -> Gpu {
        Gpu {
            name: "NVIDIA GeForce RTX 5070 Ti".to_owned(),
            api: "Vulkan",
        }
    }

    #[test]
    fn the_plan_names_a_discrete_gpu_or_the_reason_for_the_cpu_and_skips_the_check_for_a_cpu_model()
    {
        for (case, uses_gpu, adapter, expected) in [
            (
                "discrete",
                true,
                Adapter::Discrete(rtx()),
                Device::Gpu(rtx()),
            ),
            (
                "integrated, software or none",
                true,
                Adapter::Unusable,
                Device::Cpu(Fallback::NoUsableGpu),
            ),
            (
                "windows",
                true,
                Adapter::Unsupported,
                Device::Cpu(Fallback::WindowsUnsupported),
            ),
            (
                "model without a GPU export",
                false,
                Adapter::Discrete(rtx()),
                Device::Cpu(Fallback::CpuModel),
            ),
        ] {
            let checked = RefCell::new(false);
            let plan = Device::plan(uses_gpu, || {
                *checked.borrow_mut() = true;
                adapter
            });
            assert_eq!(plan, expected, "{case}");
            assert_eq!(*checked.borrow(), uses_gpu, "{case}: adapter checked");
        }
    }

    #[tokio::test]
    async fn a_load_runs_on_the_gpu_and_falls_back_to_the_cpu_with_the_reason_when_the_gpu_cannot()
    {
        type Case = (
            &'static str,
            Device,
            Option<Result<(), Attempt<&'static str>>>,
            Result<&'static str, &'static str>,
        );
        let cases: [Case; 7] = [
            ("gpu works", Device::Gpu(rtx()), Some(Ok(())), Ok("gpu")),
            (
                "driver fails",
                Device::Gpu(rtx()),
                Some(Err(Attempt::Fallback(Fallback::DriverFailed))),
                Ok("cpu: the GPU driver failed to start"),
            ),
            (
                "placement fails",
                Device::Gpu(rtx()),
                Some(Err(Attempt::Fallback(Fallback::CannotRunFully))),
                Ok("cpu: the model can't run fully on the GPU"),
            ),
            (
                "files cannot be fetched",
                Device::Gpu(rtx()),
                Some(Err(Attempt::Failed("Could not download."))),
                Err("Could not download."),
            ),
            (
                "no usable gpu",
                Device::Cpu(Fallback::NoUsableGpu),
                None,
                Ok("cpu: no usable GPU"),
            ),
            (
                "cpu model",
                Device::Cpu(Fallback::CpuModel),
                None,
                Ok("cpu: this model runs on the CPU"),
            ),
            (
                "windows",
                Device::Cpu(Fallback::WindowsUnsupported),
                None,
                Ok("cpu: GPU recognition isn't supported on Windows yet"),
            ),
        ];
        for (case, plan, gpu, expected) in cases {
            let tried_gpu = RefCell::new(false);
            let loaded: Result<String, &str> = load(
                plan,
                |_| {
                    *tried_gpu.borrow_mut() = true;
                    let answer = gpu.expect("the plan sends no CPU load to the GPU");
                    std::future::ready(answer.map(|()| "gpu".to_owned()))
                },
                |reason| std::future::ready(Ok(format!("cpu: {}", reason.reason()))),
            )
            .await;
            assert_eq!(
                loaded.as_deref().map_err(|error| *error),
                expected,
                "{case}"
            );
            assert_eq!(
                *tried_gpu.borrow(),
                matches!(
                    case,
                    "gpu works" | "driver fails" | "placement fails" | "files cannot be fetched"
                ),
                "{case}: GPU tried"
            );
        }
    }

    fn nodes(gpu_count: usize, cpu_counts: &[(&str, usize)]) -> Vec<Placed> {
        let node = |operator: &str, on_cpu| Placed {
            operator: operator.to_owned(),
            on_cpu,
        };
        let mut nodes = vec![node("Add", false); gpu_count];
        for (operator, count) in cpu_counts {
            nodes.extend(std::iter::repeat_n(node(operator, true), *count));
        }
        nodes
    }

    #[test]
    fn a_model_runs_fully_on_the_gpu_when_the_cpu_keeps_only_shape_work_for_at_most_two_percent_of_nodes()
     {
        for (case, placed, expected) in [
            ("all on the GPU", nodes(100, &[]), true),
            (
                "two percent of shape ops",
                nodes(98, &[("Unsqueeze", 2)]),
                true,
            ),
            (
                "three percent of shape ops",
                nodes(97, &[("Unsqueeze", 3)]),
                false,
            ),
            ("one MatMul", nodes(999, &[("MatMul", 1)]), false),
            ("an int8 MatMul", nodes(999, &[("MatMulInteger", 1)]), false),
            (
                "a quantize op",
                nodes(999, &[("DynamicQuantizeLinear", 1)]),
                false,
            ),
            ("a convolution", nodes(999, &[("Conv", 1)]), false),
            ("an LSTM", nodes(999, &[("LSTM", 1)]), false),
            ("a softmax", nodes(999, &[("Softmax", 1)]), false),
            (
                "a layer norm",
                nodes(999, &[("LayerNormalization", 1)]),
                false,
            ),
            ("a gemm", nodes(999, &[("Gemm", 1)]), false),
        ] {
            assert_eq!(runs_fully_on_gpu(&placed), expected, "{case}");
        }
    }

    #[test]
    fn settings_say_what_runs_where_now_or_after_the_next_load() {
        let lines = |device: Device| (device.line(true), device.line(false));
        assert_eq!(
            lines(Device::Gpu(rtx())),
            (
                "Runs on the GPU (NVIDIA GeForce RTX 5070 Ti, Vulkan)".to_owned(),
                "Will run on the GPU (NVIDIA GeForce RTX 5070 Ti, Vulkan)".to_owned()
            )
        );
        for (fallback, reason) in [
            (Fallback::NoUsableGpu, "no usable GPU"),
            (Fallback::DriverFailed, "the GPU driver failed to start"),
            (
                Fallback::CannotRunFully,
                "the model can't run fully on the GPU",
            ),
            (Fallback::CpuModel, "this model runs on the CPU"),
            (
                Fallback::WindowsUnsupported,
                "GPU recognition isn't supported on Windows yet",
            ),
        ] {
            assert_eq!(
                lines(Device::Cpu(fallback)),
                (
                    format!("Runs on the CPU: {reason}"),
                    format!("Will run on the CPU: {reason}")
                )
            );
        }
    }

    #[test]
    fn the_device_a_model_loaded_on_is_remembered_by_its_label_only() {
        loaded("device-test-model", &Device::Cpu(Fallback::NoUsableGpu));
        assert_eq!(
            loaded_on("device-test-model"),
            Some(Device::Cpu(Fallback::NoUsableGpu))
        );
        assert_eq!(loaded_on("another-model"), None);
    }
}
