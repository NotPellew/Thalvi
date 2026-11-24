//! The lazily started thread that draws the Settings overlay preview off the UI thread.

use std::sync::Arc;
use std::sync::{Mutex, OnceLock, PoisonError, mpsc};

use thalvi_desktop::theme::Style;
use tokio::sync::oneshot;

use crate::overlay::{Renderer, View};

const FAILED: &str = "The overlay preview could not be drawn.";

struct Job {
    view: View,
    style: Arc<Style>,
    dark: bool,
    scale: f32,
    reply: oneshot::Sender<Option<Vec<u8>>>,
}

static THREAD: OnceLock<Mutex<Option<mpsc::Sender<Job>>>> = OnceLock::new();

/// Draws `view` on the preview thread and returns its framed RGBA bytes.
///
/// # Errors
/// Why the preview could not be drawn, in plain words.
pub async fn draw(
    view: View,
    style: Arc<Style>,
    dark: bool,
    scale: f32,
) -> Result<Vec<u8>, String> {
    let (reply, done) = oneshot::channel();
    let job = Job {
        view,
        style,
        dark,
        scale,
        reply,
    };
    let sender = THREAD.get_or_init(|| Mutex::new(None));
    {
        let mut guard = sender.lock().unwrap_or_else(PoisonError::into_inner);
        if guard.is_none() {
            let (jobs, incoming) = mpsc::channel();
            std::thread::Builder::new()
                .name("thalvi-preview".to_owned())
                .spawn(move || run(incoming))
                .map_err(|_| FAILED.to_owned())?;
            *guard = Some(jobs);
        }
        guard
            .as_ref()
            .ok_or_else(|| FAILED.to_owned())?
            .send(job)
            .map_err(|_| FAILED.to_owned())?;
    }
    done.await
        .map_err(|_| FAILED.to_owned())?
        .ok_or_else(|| FAILED.to_owned())
}

fn run(jobs: mpsc::Receiver<Job>) {
    let mut renderer = Renderer::default();
    for job in jobs {
        let bytes = renderer
            .draw(&job.view, job.style, job.dark, job.scale)
            .map(|(width, height, rgba)| frame_bytes(width, height, &rgba));
        let _ = job.reply.send(bytes);
    }
}

/// Width and height as little-endian u32s, then straight-alpha RGBA rows.
fn frame_bytes(width: u32, height: u32, rgba: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(8 + rgba.len());
    bytes.extend_from_slice(&width.to_le_bytes());
    bytes.extend_from_slice(&height.to_le_bytes());
    bytes.extend_from_slice(rgba);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::Sample;

    #[test]
    fn an_unknown_preview_state_is_refused_with_a_plain_message() {
        assert_eq!(
            "bogus".parse::<Sample>().unwrap_err(),
            "The overlay preview state must be live, quiet, working, pending, done or failed."
        );
        for state in ["live", "quiet", "working", "pending", "done", "failed"] {
            assert!(state.parse::<Sample>().is_ok(), "{state}");
        }
    }

    #[test]
    fn a_preview_frame_has_a_header_and_straight_alpha_rgba_bytes() {
        let rgba: Vec<u8> = (0..24).collect();
        let bytes = frame_bytes(2, 3, &rgba);
        assert_eq!(&bytes[..4], &2_u32.to_le_bytes());
        assert_eq!(&bytes[4..8], &3_u32.to_le_bytes());
        assert_eq!(&bytes[8..], rgba.as_slice());
        assert_eq!(bytes.len(), 8 + 2 * 3 * 4);
    }
}
