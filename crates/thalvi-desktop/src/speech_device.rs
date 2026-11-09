//! Owns what Settings says about the hardware the catalog speech models run on.

use std::sync::{Arc, Mutex, PoisonError};

use crate::device::{Adapter, Adapters, Device, Export, Fallback};
use crate::local::LocalRecognizer;
use crate::models::{CatalogSpeech, ModelStore};
use crate::transducer::TransducerSource;

/// The model the engine has loaded, and what the latest check of the graphics adapter found.
pub struct SpeechDevices {
    adapters: Adapters,
    active: Mutex<Option<Arc<LocalRecognizer<TransducerSource>>>>,
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
    pub fn use_recognizer(&self, local: Option<Arc<LocalRecognizer<TransducerSource>>>) {
        *self.active.lock().unwrap_or_else(PoisonError::into_inner) = local;
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
            .clone()
            .filter(|local| local.name() == model.name())
            .and_then(|local| local.device());
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
}
