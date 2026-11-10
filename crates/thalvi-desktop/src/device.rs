//! Owns which hardware on-device recognition runs on: the adapter check, the load order, and the reasons for the CPU.

use std::fmt::Display;
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Value, json};

use crate::models::SpeechProgress;

#[cfg(target_os = "linux")]
mod webgpu;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
use self::webgpu as native;
#[cfg(target_os = "windows")]
use self::windows as native;

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
    /// The GPU's files are not on disk, and the CPU's are.
    GpuFilesMissing,
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
            Self::GpuFilesMissing => "the GPU model isn't downloaded",
            Self::CpuModel => "this model runs on the CPU",
            Self::WindowsUnsupported => "GPU recognition isn't supported on Windows yet",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::NoUsableGpu => "no_usable_gpu",
            Self::DriverFailed => "driver_failed",
            Self::CannotRunFully => "cannot_run_fully",
            Self::GpuFilesMissing => "gpu_files_missing",
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

    /// Where a plan goes given what is on disk: a GPU plan loads on the CPU, at once, when only the CPU's files are there.
    #[must_use]
    pub fn given_files(self, gpu_files: bool, cpu_files: bool) -> Self {
        match self {
            Self::Gpu(_) if !gpu_files && cpu_files => Self::Cpu(Fallback::GpuFilesMissing),
            plan => plan,
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

/// The result of the latest adapter check, kept for Settings. Each load checks again; Settings checks only when
/// no load has yet.
#[derive(Clone)]
pub struct Adapters {
    check: Arc<dyn Fn() -> Adapter + Send + Sync>,
    last: Arc<Mutex<Option<Adapter>>>,
}

impl Default for Adapters {
    fn default() -> Self {
        Self::checking(native::adapter)
    }
}

impl Adapters {
    /// Checks with `check`, which a test replaces with a count.
    #[must_use]
    pub fn checking(check: impl Fn() -> Adapter + Send + Sync + 'static) -> Self {
        Self {
            check: Arc::new(check),
            last: Arc::default(),
        }
    }

    /// Checks the default adapter of this computer now and remembers what it found.
    #[must_use]
    pub fn check(&self) -> Adapter {
        let found = (self.check)();
        *self.last.lock().unwrap_or_else(PoisonError::into_inner) = Some(found.clone());
        found
    }

    /// What the latest check found, or a check now when there has been none; callers at once share that one check.
    #[must_use]
    pub fn last_or_check(&self) -> Adapter {
        let mut last = self.last.lock().unwrap_or_else(PoisonError::into_inner);
        last.get_or_insert_with(|| (self.check)()).clone()
    }
}

/// Which export a step of a load reads or fetches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Export {
    /// The fp16 files a GPU runs.
    Gpu,
    /// The int8 files the CPU runs.
    Cpu,
}

/// What a load is doing now, for the notice shown while it runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Reading the files from disk, checking them as it goes.
    Reading,
    /// Downloading an export that is not on disk, before the first percent has arrived.
    Fetching(Export),
    /// A download of an export, with the whole percent of its bytes that have arrived.
    Downloading(Export, u8),
}

/// How an attempt to load on the GPU failed.
#[derive(Debug, PartialEq, Eq)]
pub enum Attempt<E> {
    /// The CPU takes over, with this reason.
    Fallback(Fallback),
    /// Not a GPU problem; the load fails as it would on the CPU.
    Failed(E),
}

impl<E> From<E> for Attempt<E> {
    fn from(error: E) -> Self {
        Self::Failed(error)
    }
}

/// What a load needs from the model it loads: its files on disk, and a session on a device.
pub trait Provider: Sync {
    type Model: Send;
    type Files: Send;
    type Error: Display + Send;

    /// Whether every file of `export` is on disk, without checking its bytes.
    fn cached(&self, export: Export) -> impl Future<Output = bool> + Send;

    /// The files of `export`, checked, and downloaded first where missing or damaged; `progress` is told how
    /// the download goes.
    fn fetch(
        &self,
        export: Export,
        progress: impl FnMut(SpeechProgress) + Send,
    ) -> impl Future<Output = Result<Self::Files, Self::Error>> + Send;

    fn open_gpu(
        &self,
        gpu: Gpu,
        files: Self::Files,
    ) -> impl Future<Output = Result<Self::Model, Attempt<Self::Error>>> + Send;

    fn open_cpu(
        &self,
        reason: Fallback,
        files: Self::Files,
    ) -> impl Future<Output = Result<Self::Model, Self::Error>> + Send;
}

/// Loads on the GPU when the plan says so and falls back to the CPU, with the reason, when that cannot be done.
///
/// With the GPU's files on disk the model loads on the GPU; with only the CPU's it loads on the CPU at once; with
/// neither, the GPU's are downloaded first, and the CPU's if that fails. A GPU session that does not start or does
/// not take the whole model falls back to the CPU too. `step` is told what each stage does, a download's percent
/// included, and goes back to reading once the files are in.
///
/// # Errors
/// The error of a load that is not about the GPU, or of the CPU load.
pub async fn load<P: Provider>(
    plan: Device,
    provider: &P,
    step: impl Fn(Step) + Send + Sync,
) -> Result<P::Model, P::Error> {
    let reason = match plan {
        Device::Cpu(reason) => reason,
        Device::Gpu(gpu) => {
            let gpu_files = provider.cached(Export::Gpu).await;
            let cpu_files = !gpu_files && provider.cached(Export::Cpu).await;
            match Device::Gpu(gpu).given_files(gpu_files, cpu_files) {
                Device::Gpu(gpu) => {
                    step(if gpu_files {
                        Step::Reading
                    } else {
                        Step::Fetching(Export::Gpu)
                    });
                    match provider
                        .fetch(Export::Gpu, downloading(Export::Gpu, &step))
                        .await
                    {
                        Ok(files) => match provider.open_gpu(gpu, files).await {
                            Ok(model) => return Ok(model),
                            Err(Attempt::Fallback(reason)) => reason,
                            Err(Attempt::Failed(error)) => return Err(error),
                        },
                        Err(error) => {
                            tracing::warn!("The GPU speech model could not be fetched: {error}");
                            Fallback::GpuFilesMissing
                        }
                    }
                }
                Device::Cpu(reason) => reason,
            }
        }
    };
    step(if provider.cached(Export::Cpu).await {
        Step::Reading
    } else {
        Step::Fetching(Export::Cpu)
    });
    let files = provider
        .fetch(Export::Cpu, downloading(Export::Cpu, &step))
        .await?;
    provider.open_cpu(reason, files).await
}

/// Turns what a download reports into steps; a download's percent is shown only while it runs.
fn downloading(
    export: Export,
    step: &(impl Fn(Step) + Send + Sync),
) -> impl FnMut(SpeechProgress) + Send {
    move |progress| match progress {
        SpeechProgress::Verifying => {}
        SpeechProgress::Downloading { percent } => step(Step::Downloading(export, percent)),
        SpeechProgress::Done => step(Step::Reading),
    }
}

/// One of the three networks of a transducer; each is a session of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    Encoder,
    Decoder,
    Joiner,
}

/// One node of a loaded session: its operator, and whether the CPU provider runs it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placed {
    pub operator: String,
    pub on_cpu: bool,
}

/// How many nodes of a session run on the GPU, of all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Nodes {
    pub on_gpu: usize,
    pub total: usize,
}

impl Nodes {
    #[must_use]
    pub fn of(placed: &[Placed]) -> Self {
        Self {
            on_gpu: placed.iter().filter(|node| !node.on_cpu).count(),
            total: placed.len(),
        }
    }
}

/// Where each session of a model loaded on the GPU put its nodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Placement {
    pub encoder: Nodes,
    pub decoder: Nodes,
    pub joiner: Nodes,
}

impl Placement {
    /// `fields` of a speech metric with the node counts of each session added.
    #[must_use]
    pub fn tag(&self, mut fields: Value) -> Value {
        if let Some(fields) = fields.as_object_mut() {
            for (part, nodes) in [
                ("encoder", self.encoder),
                ("decoder", self.decoder),
                ("joiner", self.joiner),
            ] {
                fields.insert(format!("{part}_nodes_on_gpu"), nodes.on_gpu.into());
                fields.insert(format!("{part}_nodes"), nodes.total.into());
            }
        }
        fields
    }
}

/// Fewest nodes of the encoder, in percent, that must run on the GPU.
const MOST_ON_GPU: usize = 98;

/// ONNX Runtime keeps shape arithmetic on the CPU, so "everything on the GPU" fails for every export. What
/// matters is that no operator that does the arithmetic of the model is among them. Of the encoder, whose
/// thousands of nodes make a slow round trip for each stray one, almost all must also be on the GPU.
#[must_use]
pub fn runs_fully_on_gpu(part: Part, nodes: &[Placed]) -> bool {
    let compute = |operator: &str| {
        matches!(
            operator,
            "MatMul" | "Gemm" | "Conv" | "LSTM" | "LayerNormalization" | "Softmax"
        ) || operator.ends_with("Integer")
            || operator.starts_with("DynamicQuantize")
    };
    let counts = Nodes::of(nodes);
    !nodes
        .iter()
        .any(|node| node.on_cpu && compute(&node.operator))
        && (part != Part::Encoder || counts.on_gpu * 100 >= counts.total * MOST_ON_GPU)
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicUsize, Ordering};

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

    #[test]
    fn a_gpu_plan_goes_to_the_cpu_at_once_only_when_just_the_cpus_files_are_on_disk() {
        for (case, plan, gpu_files, cpu_files, expected) in [
            (
                "both exports",
                Device::Gpu(rtx()),
                true,
                false,
                Device::Gpu(rtx()),
            ),
            (
                "only the CPU's",
                Device::Gpu(rtx()),
                false,
                true,
                Device::Cpu(Fallback::GpuFilesMissing),
            ),
            (
                "nothing",
                Device::Gpu(rtx()),
                false,
                false,
                Device::Gpu(rtx()),
            ),
            (
                "a CPU plan keeps its reason",
                Device::Cpu(Fallback::NoUsableGpu),
                false,
                true,
                Device::Cpu(Fallback::NoUsableGpu),
            ),
        ] {
            assert_eq!(plan.given_files(gpu_files, cpu_files), expected, "{case}");
        }
    }

    /// Provides `String` models; what is on disk, and what fails, is set per case, and every call is logged.
    struct Fake {
        gpu_cached: bool,
        cpu_cached: bool,
        gpu_fetch: Result<(), &'static str>,
        cpu_fetch: Result<(), &'static str>,
        gpu_open: Result<(), Attempt<&'static str>>,
        /// What every successful fetch reports as it goes.
        reports: Vec<SpeechProgress>,
        calls: std::sync::Mutex<Vec<&'static str>>,
    }

    impl Fake {
        fn log(&self, call: &'static str) {
            self.calls
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(call);
        }
    }

    impl Provider for Fake {
        type Model = String;
        type Files = Export;
        type Error = &'static str;

        fn cached(&self, export: Export) -> impl Future<Output = bool> + Send {
            std::future::ready(match export {
                Export::Gpu => self.gpu_cached,
                Export::Cpu => self.cpu_cached,
            })
        }

        fn fetch(
            &self,
            export: Export,
            mut progress: impl FnMut(SpeechProgress) + Send,
        ) -> impl Future<Output = Result<Export, &'static str>> + Send {
            let (call, answer) = match export {
                Export::Gpu => ("fetch gpu", self.gpu_fetch),
                Export::Cpu => ("fetch cpu", self.cpu_fetch),
            };
            self.log(call);
            if answer.is_ok() {
                self.reports.iter().for_each(|report| progress(*report));
            }
            std::future::ready(answer.map(|()| export))
        }

        fn open_gpu(
            &self,
            _gpu: Gpu,
            _files: Export,
        ) -> impl Future<Output = Result<String, Attempt<&'static str>>> + Send {
            self.log("open gpu");
            let answer = match &self.gpu_open {
                Ok(()) => Ok("gpu".to_owned()),
                Err(Attempt::Fallback(reason)) => Err(Attempt::Fallback(*reason)),
                Err(Attempt::Failed(error)) => Err(Attempt::Failed(*error)),
            };
            std::future::ready(answer)
        }

        fn open_cpu(
            &self,
            reason: Fallback,
            files: Export,
        ) -> impl Future<Output = Result<String, &'static str>> + Send {
            self.log("open cpu");
            std::future::ready(Ok(format!("cpu of {files:?}: {}", reason.reason())))
        }
    }

    fn fake() -> Fake {
        Fake {
            gpu_cached: false,
            cpu_cached: false,
            gpu_fetch: Ok(()),
            cpu_fetch: Ok(()),
            gpu_open: Ok(()),
            reports: Vec::new(),
            calls: std::sync::Mutex::default(),
        }
    }

    impl Fake {
        /// Which exports are on disk.
        fn disk(self, gpu_cached: bool, cpu_cached: bool) -> Self {
            Self {
                gpu_cached,
                cpu_cached,
                ..self
            }
        }

        /// What every successful fetch reports.
        fn reports(self, reports: &[SpeechProgress]) -> Self {
            Self {
                reports: reports.to_vec(),
                ..self
            }
        }

        fn gpu_open_fails(self, failure: Attempt<&'static str>) -> Self {
            Self {
                gpu_open: Err(failure),
                ..self
            }
        }
    }

    /// Loads on `plan` and checks the model, the steps told, and the calls made.
    async fn check(
        case: &str,
        plan: Device,
        provider: Fake,
        expected: Result<&str, &str>,
        steps: &[Step],
        calls: &[&str],
    ) {
        let seen = std::sync::Mutex::new(Vec::new());
        let loaded = load(plan, &provider, |step| {
            seen.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(step);
        })
        .await;
        assert_eq!(
            loaded.as_ref().map(String::as_str).map_err(|error| *error),
            expected,
            "{case}"
        );
        assert_eq!(
            *seen.lock().unwrap_or_else(PoisonError::into_inner),
            steps,
            "{case}: steps"
        );
        assert_eq!(
            *provider
                .calls
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
            calls,
            "{case}: calls"
        );
    }

    const MISSING: &str = "cpu of Cpu: the GPU model isn't downloaded";
    const DRIVER: &str = "cpu of Cpu: the GPU driver failed to start";

    #[tokio::test]
    async fn a_gpu_plan_loads_on_what_is_on_disk_and_falls_back_to_the_cpu_when_the_gpu_files_or_the_gpu_fail()
     {
        use Export::{Cpu, Gpu as OnGpu};
        use Step::{Fetching, Reading};
        let gpu = || Device::Gpu(rtx());
        let driver = || Attempt::Fallback(Fallback::DriverFailed);
        check(
            "gpu files on disk",
            gpu(),
            fake().disk(true, true),
            Ok("gpu"),
            &[Reading],
            &["fetch gpu", "open gpu"],
        )
        .await;
        check(
            "only the cpu files on disk: the cpu now, nothing downloaded",
            gpu(),
            fake().disk(false, true),
            Ok(MISSING),
            &[Reading],
            &["fetch cpu", "open cpu"],
        )
        .await;
        check(
            "nothing on disk: the gpu files are downloaded and used",
            gpu(),
            fake(),
            Ok("gpu"),
            &[Fetching(OnGpu)],
            &["fetch gpu", "open gpu"],
        )
        .await;
        check(
            "nothing on disk and the gpu download fails: the cpu files are downloaded",
            gpu(),
            Fake {
                gpu_fetch: Err("offline"),
                ..fake()
            },
            Ok(MISSING),
            &[Fetching(OnGpu), Fetching(Cpu)],
            &["fetch gpu", "fetch cpu", "open cpu"],
        )
        .await;
        check(
            "nothing on disk and both downloads fail",
            gpu(),
            Fake {
                gpu_fetch: Err("offline"),
                cpu_fetch: Err("still offline"),
                ..fake()
            },
            Err("still offline"),
            &[Fetching(OnGpu), Fetching(Cpu)],
            &["fetch gpu", "fetch cpu"],
        )
        .await;
        check(
            "the gpu session fails to start with the cpu files on disk",
            gpu(),
            fake().disk(true, true).gpu_open_fails(driver()),
            Ok(DRIVER),
            &[Reading, Reading],
            &["fetch gpu", "open gpu", "fetch cpu", "open cpu"],
        )
        .await;
        check(
            "the gpu session fails to start and the cpu files must be downloaded",
            gpu(),
            fake().disk(true, false).gpu_open_fails(driver()),
            Ok(DRIVER),
            &[Reading, Fetching(Cpu)],
            &["fetch gpu", "open gpu", "fetch cpu", "open cpu"],
        )
        .await;
        check(
            "the gpu does not take the whole model",
            gpu(),
            fake()
                .disk(true, true)
                .gpu_open_fails(Attempt::Fallback(Fallback::CannotRunFully)),
            Ok("cpu of Cpu: the model can't run fully on the GPU"),
            &[Reading, Reading],
            &["fetch gpu", "open gpu", "fetch cpu", "open cpu"],
        )
        .await;
        check(
            "the gpu load fails for a reason that is not the gpu's",
            gpu(),
            fake()
                .disk(true, true)
                .gpu_open_fails(Attempt::Failed("The tokens could not be read.")),
            Err("The tokens could not be read."),
            &[Reading],
            &["fetch gpu", "open gpu"],
        )
        .await;
    }

    #[tokio::test]
    async fn a_cpu_plan_loads_the_cpu_files_with_its_reason_and_never_touches_the_gpu_files() {
        use Export::Cpu;
        use Step::{Fetching, Reading};
        for fallback in [
            Fallback::NoUsableGpu,
            Fallback::CpuModel,
            Fallback::WindowsUnsupported,
        ] {
            let expected = format!("cpu of Cpu: {}", fallback.reason());
            check(
                "files on disk",
                Device::Cpu(fallback),
                fake().disk(false, true),
                Ok(&expected),
                &[Reading],
                &["fetch cpu", "open cpu"],
            )
            .await;
            check(
                "files to download",
                Device::Cpu(fallback),
                fake(),
                Ok(&expected),
                &[Fetching(Cpu)],
                &["fetch cpu", "open cpu"],
            )
            .await;
        }
        check(
            "files that cannot be downloaded",
            Device::Cpu(Fallback::NoUsableGpu),
            Fake {
                cpu_fetch: Err("offline"),
                ..fake()
            },
            Err("offline"),
            &[Fetching(Cpu)],
            &["fetch cpu"],
        )
        .await;
    }

    #[tokio::test]
    async fn a_download_shows_its_percent_by_export_and_goes_back_to_reading_once_the_files_are_in()
    {
        use Export::{Cpu, Gpu as OnGpu};
        use SpeechProgress::{Done, Downloading, Verifying};
        let reports = [
            Verifying,
            Downloading { percent: 0 },
            Downloading { percent: 42 },
            Downloading { percent: 100 },
            Done,
        ];
        check(
            "the CPU files",
            Device::Cpu(Fallback::CpuModel),
            fake().reports(&reports),
            Ok("cpu of Cpu: this model runs on the CPU"),
            &[
                Step::Fetching(Cpu),
                Step::Downloading(Cpu, 0),
                Step::Downloading(Cpu, 42),
                Step::Downloading(Cpu, 100),
                Step::Reading,
            ],
            &["fetch cpu", "open cpu"],
        )
        .await;
        check(
            "the GPU files",
            Device::Gpu(rtx()),
            fake().reports(&reports),
            Ok("gpu"),
            &[
                Step::Fetching(OnGpu),
                Step::Downloading(OnGpu, 0),
                Step::Downloading(OnGpu, 42),
                Step::Downloading(OnGpu, 100),
                Step::Reading,
            ],
            &["fetch gpu", "open gpu"],
        )
        .await;
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
    fn no_session_may_leave_compute_on_the_cpu_and_the_encoder_alone_must_have_98_percent_of_its_nodes_on_the_gpu()
     {
        for (case, placed, encoder, others) in [
            ("all on the GPU", nodes(100, &[]), true, true),
            (
                "two percent of shape ops",
                nodes(98, &[("Unsqueeze", 2)]),
                true,
                true,
            ),
            (
                "three percent of shape ops",
                nodes(97, &[("Unsqueeze", 3)]),
                false,
                true,
            ),
            (
                "half shape ops, as the decoder has",
                nodes(50, &[("Shape", 50)]),
                false,
                true,
            ),
            ("one MatMul", nodes(999, &[("MatMul", 1)]), false, false),
            (
                "an int8 MatMul",
                nodes(999, &[("MatMulInteger", 1)]),
                false,
                false,
            ),
            (
                "a quantize op",
                nodes(999, &[("DynamicQuantizeLinear", 1)]),
                false,
                false,
            ),
            ("a convolution", nodes(999, &[("Conv", 1)]), false, false),
            ("an LSTM", nodes(999, &[("LSTM", 1)]), false, false),
            ("a softmax", nodes(999, &[("Softmax", 1)]), false, false),
            (
                "a layer norm",
                nodes(999, &[("LayerNormalization", 1)]),
                false,
                false,
            ),
            ("a gemm", nodes(999, &[("Gemm", 1)]), false, false),
        ] {
            assert_eq!(
                runs_fully_on_gpu(Part::Encoder, &placed),
                encoder,
                "{case}: encoder"
            );
            for part in [Part::Decoder, Part::Joiner] {
                assert_eq!(runs_fully_on_gpu(part, &placed), others, "{case}: {part:?}");
            }
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
            (Fallback::GpuFilesMissing, "the GPU model isn't downloaded"),
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
    fn a_metric_carries_the_node_counts_of_each_session() {
        let placement = Placement {
            encoder: Nodes::of(&nodes(98, &[("Shape", 2)])),
            decoder: Nodes {
                on_gpu: 5,
                total: 9,
            },
            joiner: Nodes {
                on_gpu: 3,
                total: 3,
            },
        };
        assert_eq!(
            placement.tag(json!({"model": "m"})),
            json!({
                "model": "m",
                "encoder_nodes_on_gpu": 98, "encoder_nodes": 100,
                "decoder_nodes_on_gpu": 5, "decoder_nodes": 9,
                "joiner_nodes_on_gpu": 3, "joiner_nodes": 3,
            })
        );
    }

    #[test]
    fn the_adapter_is_checked_by_each_check_and_by_settings_only_while_there_is_no_result() {
        let counted = Arc::new(AtomicUsize::new(0));
        let adapters = Adapters::checking({
            let counted = Arc::clone(&counted);
            move || {
                counted.fetch_add(1, Ordering::SeqCst);
                Adapter::Discrete(rtx())
            }
        });
        let asked = || counted.load(Ordering::SeqCst);

        for _card in 0..3 {
            assert_eq!(adapters.last_or_check(), Adapter::Discrete(rtx()));
        }
        assert_eq!(asked(), 1, "three cards, one check");

        assert_eq!(adapters.check(), Adapter::Discrete(rtx()));
        assert_eq!(asked(), 2, "a load checks again");
        assert_eq!(adapters.last_or_check(), Adapter::Discrete(rtx()));
        assert_eq!(asked(), 2, "and Settings then reads that result");
    }

    #[test]
    fn cards_asking_at_once_share_the_one_check() {
        let counted = Arc::new(AtomicUsize::new(0));
        let adapters = Adapters::checking({
            let counted = Arc::clone(&counted);
            move || {
                counted.fetch_add(1, Ordering::SeqCst);
                for _ in 0..1_000 {
                    std::thread::yield_now();
                }
                Adapter::Unusable
            }
        });
        let together = std::sync::Barrier::new(8);
        std::thread::scope(|scope| {
            for _card in 0..8 {
                scope.spawn(|| {
                    together.wait();
                    assert_eq!(adapters.last_or_check(), Adapter::Unusable);
                });
            }
        });
        assert_eq!(counted.load(Ordering::SeqCst), 1);
    }
}
