//! Owns what Settings says about the hardware the catalog speech models run on.

use std::sync::{Arc, Mutex, PoisonError};

use crate::device::{Adapter, Adapters, Device, Export, Fallback};
use crate::local::{LocalRecognizer, ModelSource};
use crate::models::{CatalogSpeech, ModelStore};

/// The recognizer whose model the engine runs: its name, and where the model runs now.
struct Active {
    name: String,
    device: Box<dyn Fn() -> Option<Device> + Send + Sync>,
}

/// The model the engine has loaded, and what the latest check of the graphics adapter found.
pub struct SpeechDevices {
    adapters: Adapters,
    active: Mutex<Option<Active>>,
}

impl SpeechDevices {
    #[must_use]
    pub fn new(adapters: Adapters) -> Self {
        Self {
            adapters,
            active: Mutex::new(None),
        }
    }

    /// The record that loads write their adapter checks to.
    #[must_use]
    pub fn adapters(&self) -> Adapters {
        self.adapters.clone()
    }

    /// Names the recognizer whose model Settings reports as the one running; `None` when the engine has none.
    pub fn use_recognizer<S: ModelSource>(&self, local: Option<Arc<LocalRecognizer<S>>>) {
        *self.active.lock().unwrap_or_else(PoisonError::into_inner) = local.map(|local| Active {
            name: local.name().to_owned(),
            device: Box::new(move || local.device()),
        });
    }

    /// Where each catalog model would load on this computer, from the latest adapter check, or one now when no
    /// load has made one.
    pub async fn plans(&self) -> impl Fn(CatalogSpeech) -> Device + use<> {
        let adapters = self.adapters.clone();
        let adapter = tokio::task::spawn_blocking(move || adapters.last_or_check())
            .await
            .unwrap_or(Adapter::Unusable);
        move |model| Device::plan(model.uses_gpu(), || adapter.clone())
    }

    /// The Settings line for `model`: where it runs if the engine has it loaded, else where it will load.
    pub async fn line(&self, store: &ModelStore, model: CatalogSpeech) -> String {
        let loaded = self
            .active
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .filter(|active| active.name == model.name())
            .and_then(|active| (active.device)());
        let planned = self.plans().await(model);
        let gpu_files = model.uses_gpu() && store.is_cached(&model.assets_of(Export::Gpu)).await;
        let cpu_files = store.is_cached(&model.assets_of(Export::Cpu)).await;
        describe(loaded, planned, gpu_files, cpu_files)
    }
}

/// The line for a model that runs on `loaded`, or, when it is not loaded, will load as `planned` with these files
/// on disk.
fn describe(loaded: Option<Device>, planned: Device, gpu_files: bool, cpu_files: bool) -> String {
    match loaded {
        Some(Device::Cpu(Fallback::GpuFilesMissing)) if gpu_files => {
            "Runs on the CPU. The GPU model is downloaded now and is used the next time the speech model loads."
                .to_owned()
        }
        Some(device) => device.line(true),
        None => planned.given_files(gpu_files, cpu_files).line(false),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::device::Gpu;

    fn rtx() -> Gpu {
        Gpu {
            name: "NVIDIA GeForce RTX 5070 Ti".to_owned(),
            api: "Vulkan",
        }
    }

    #[test]
    fn the_line_follows_the_loaded_model_and_never_a_stale_one() {
        let on_gpu = || Device::Gpu(rtx());
        let missing = || Device::Cpu(Fallback::GpuFilesMissing);
        let gpu_line = "Runs on the GPU (NVIDIA GeForce RTX 5070 Ti, Vulkan)";
        let will_gpu = "Will run on the GPU (NVIDIA GeForce RTX 5070 Ti, Vulkan)";
        for (case, loaded, planned, gpu_files, cpu_files, expected) in [
            (
                "loaded on the GPU",
                Some(on_gpu()),
                on_gpu(),
                true,
                false,
                gpu_line,
            ),
            (
                "loaded on the CPU after the driver failed",
                Some(Device::Cpu(Fallback::DriverFailed)),
                on_gpu(),
                true,
                true,
                "Runs on the CPU: the GPU driver failed to start",
            ),
            (
                "loaded on the CPU for the missing GPU files, still missing",
                Some(missing()),
                on_gpu(),
                false,
                true,
                "Runs on the CPU: the GPU model isn't downloaded",
            ),
            (
                "loaded on the CPU for the missing GPU files, downloaded since",
                Some(missing()),
                on_gpu(),
                true,
                true,
                "Runs on the CPU. The GPU model is downloaded now and is used the next time the speech model loads.",
            ),
            (
                "not loaded, GPU files on disk",
                None,
                on_gpu(),
                true,
                true,
                will_gpu,
            ),
            (
                "not loaded, only the CPU files on disk",
                None,
                on_gpu(),
                false,
                true,
                "Will run on the CPU: the GPU model isn't downloaded",
            ),
            (
                "not loaded, nothing on disk",
                None,
                on_gpu(),
                false,
                false,
                will_gpu,
            ),
            (
                "not loaded, no usable GPU",
                None,
                Device::Cpu(Fallback::NoUsableGpu),
                false,
                true,
                "Will run on the CPU: no usable GPU",
            ),
        ] {
            assert_eq!(
                describe(loaded, planned, gpu_files, cpu_files),
                expected,
                "{case}"
            );
        }
    }

    fn devices(checks: &Arc<AtomicUsize>) -> SpeechDevices {
        let checks = Arc::clone(checks);
        SpeechDevices::new(Adapters::checking(move || {
            checks.fetch_add(1, Ordering::SeqCst);
            Adapter::Discrete(rtx())
        }))
    }

    fn seed(dir: &std::path::Path, assets: &crate::models::SpeechAssets<'_>) {
        for asset in [assets.encoder, assets.decoder, assets.joiner, assets.tokens]
            .into_iter()
            .chain(assets.encoder_data)
        {
            let path = dir.join(asset.file_name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::File::create(path)
                .unwrap()
                .set_len(asset.size)
                .unwrap();
        }
    }

    #[tokio::test]
    async fn one_check_plans_every_model_and_only_the_gpu_model_gets_the_gpu() {
        let checks = Arc::new(AtomicUsize::new(0));
        let devices = devices(&checks);
        for _settings_opened in 0..2 {
            let plans = devices.plans().await;
            let planned: Vec<_> = CatalogSpeech::ALL.map(&plans).into_iter().collect();
            let expected: Vec<_> = CatalogSpeech::ALL
                .map(|model| {
                    if model == CatalogSpeech::Unified160ms {
                        Device::Gpu(rtx())
                    } else {
                        Device::Cpu(Fallback::CpuModel)
                    }
                })
                .into_iter()
                .collect();
            assert_eq!(planned, expected);
        }
        assert_eq!(checks.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_model_the_engine_has_not_loaded_will_run_where_its_files_on_disk_let_it() {
        let dir = std::env::temp_dir().join(format!("thalvi-line-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = ModelStore::in_dir(&dir);
        let devices = devices(&Arc::new(AtomicUsize::new(0)));
        let line = async |model| devices.line(&store, model).await;
        let unified = CatalogSpeech::Unified160ms;
        let will_gpu = "Will run on the GPU (NVIDIA GeForce RTX 5070 Ti, Vulkan)";

        assert_eq!(line(unified).await, will_gpu, "nothing downloaded yet");
        seed(&dir, &unified.assets());
        assert_eq!(
            line(unified).await,
            "Will run on the CPU: the GPU model isn't downloaded",
            "only the int8 files"
        );
        seed(&dir, &unified.assets_of(Export::Gpu));
        assert_eq!(line(unified).await, will_gpu, "the GPU files too");
        assert_eq!(
            line(CatalogSpeech::Nemotron160ms).await,
            "Will run on the CPU: this model runs on the CPU"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A model that runs on `device` and does nothing else.
    struct Source(Device);

    struct Model(Device);

    struct Stream;

    impl crate::local::SpeechStream for Stream {
        fn accept(
            &mut self,
            _frame: &crate::frame::Frame,
        ) -> Result<Vec<crate::recognizer::RecognizerEvent>, crate::recognizer::RecognizerError>
        {
            Ok(Vec::new())
        }
    }

    impl crate::local::SpeechModel for Model {
        type Stream = Stream;

        fn device(&self) -> Device {
            self.0.clone()
        }

        fn stream(&self) -> Stream {
            Stream
        }
    }

    impl ModelSource for Source {
        type Model = Model;

        fn load(
            &self,
            _step: impl Fn(crate::device::Step) + Send + Sync,
        ) -> impl std::future::Future<Output = Result<Model, crate::recognizer::RecognizerError>> + Send
        {
            std::future::ready(Ok(Model(self.0.clone())))
        }
    }

    fn recognizer(name: &str, device: Device) -> Arc<LocalRecognizer<Source>> {
        Arc::new(LocalRecognizer::new(
            Source(device),
            name,
            crate::metrics::Recorder::disabled().metrics(),
        ))
    }

    #[tokio::test]
    async fn settings_say_where_the_model_the_engine_loaded_runs_and_nothing_stale_after_it_is_replaced()
     {
        let dir = std::env::temp_dir().join(format!("thalvi-active-{}", std::process::id()));
        let store = ModelStore::in_dir(&dir);
        let checks = Arc::new(AtomicUsize::new(0));
        let devices = devices(&checks);
        let line = async || devices.line(&store, CatalogSpeech::Unified160ms).await;
        let will_gpu = "Will run on the GPU (NVIDIA GeForce RTX 5070 Ti, Vulkan)";

        let local = recognizer("unified_160ms", Device::Cpu(Fallback::DriverFailed));
        devices.use_recognizer(Some(Arc::clone(&local)));
        assert_eq!(line().await, will_gpu, "named but not loaded yet");
        local.load().await.unwrap();
        assert_eq!(
            line().await,
            "Runs on the CPU: the GPU driver failed to start"
        );

        devices.use_recognizer(Some(recognizer("nemotron_160ms", Device::Gpu(rtx()))));
        assert_eq!(line().await, will_gpu, "another model is the active one");
        devices.use_recognizer::<Source>(None);
        assert_eq!(line().await, will_gpu, "no engine model");

        let _ = devices.adapters().check();
        let before = checks.load(Ordering::SeqCst);
        let _ = line().await;
        assert_eq!(
            checks.load(Ordering::SeqCst),
            before,
            "the check a load made is the one Settings reads"
        );
    }
}
