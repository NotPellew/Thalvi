//! Runs the capture engine on the default microphone and prints what it hears; it delivers nothing.

use std::path::PathBuf;
use std::pin::pin;
use std::process::ExitCode;
use std::time::Instant;

use serde_json::Value;
use thalvi_desktop::audio::{InputGate, Microphone};
use thalvi_desktop::config::DesktopConfig;
use thalvi_desktop::engine::{
    CaptureOutcome, CpalCuePlayer, Engine, EngineState, Parts, WakeTrigger,
};
use thalvi_desktop::metrics::{Event, Recorder};
use thalvi_desktop::models::ModelStore;
use thalvi_desktop::recognizer::ServerRecognizer;
use thalvi_desktop::wake::WakeEngine;
use tokio::task::LocalSet;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("{message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut args = std::env::args_os().skip(1);
    let path = args.next().map(PathBuf::from);
    if args.next().is_some() {
        return Err("Usage: thalvi-desktop [config-file]".to_owned());
    }
    let record = std::env::var("THALVI_RECORD").ok();
    let config = if let Some(path) = path {
        DesktopConfig::load(&path, record.as_deref())
    } else {
        let default = dirs::config_dir()
            .ok_or("This system has no per-user config directory.")?
            .join("thalvi")
            .join("desktop.json");
        if default.exists() {
            DesktopConfig::load(&default, record.as_deref())
        } else {
            DesktopConfig::from_json_str("{}", record.as_deref())
        }
    }
    .map_err(|error| error.to_string())?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("Thalvi could not start: {error}"))?;
    LocalSet::new().block_on(&runtime, listen_and_print(config))
}

fn describe(state: &EngineState) -> String {
    match state {
        EngineState::Listening => "Listening for a wake word.".to_owned(),
        EngineState::StartingCapture => "Wake word heard. Starting capture.".to_owned(),
        EngineState::Capturing { committed, partial } => format!(
            "Capturing: {committed}{}",
            partial
                .as_ref()
                .map_or_else(String::new, |partial| format!(" (hearing: {partial})"))
        ),
        EngineState::PendingEnding {
            route,
            text,
            waiting_for_quiet,
        } => format!(
            "Ending {} pending ({}): {text}",
            format!("{route:?}").to_lowercase(),
            if *waiting_for_quiet {
                "waiting for you to stop speaking"
            } else {
                "stay quiet to confirm, speak to continue"
            }
        ),
        EngineState::TranscribingSecondary { .. } => {
            "Transcribing the capture a second time.".to_owned()
        }
    }
}

fn report(outcome: &CaptureOutcome) {
    match outcome {
        CaptureOutcome::Accepted(request) => println!(
            "Accepted ({}): {}",
            format!("{:?}", request.route).to_lowercase(),
            request.text
        ),
        CaptureOutcome::Cancelled => println!("Capture cancelled."),
        CaptureOutcome::Failed {
            error,
            recoverable_text,
        } => {
            println!("Capture failed: {error}");
            if let Some(text) = recoverable_text {
                println!("Dictated so far: {text}");
            }
        }
    }
}

async fn listen_and_print(config: DesktopConfig) -> Result<(), String> {
    let recorder = Recorder::per_user(config.record).map_err(|error| error.to_string())?;
    let store = ModelStore::per_user().map_err(|error| error.to_string())?;
    println!("Loading the wake word models.");
    let started = Instant::now();
    let wake = WakeEngine::load(&config.wake, &store).await;
    recorder.metric(
        Event::WakeLoad,
        if wake.is_ok() { "ok" } else { "error" },
        Some(started.elapsed()),
        Value::Null,
    );
    let wake = wake.map_err(|error| error.to_string())?;

    let gate = InputGate::default();
    let mut microphone = Microphone::new(gate.clone());
    let (microphone, frames) = tokio::task::spawn_blocking(move || {
        let frames = microphone.open();
        (microphone, frames)
    })
    .await
    .map_err(|error| format!("Thalvi could not open the microphone: {error}"))?;
    let mut frames = frames.map_err(|error| error.to_string())?;

    let http = reqwest::Client::builder()
        .build()
        .map_err(|error| format!("Thalvi could not start: {error}"))?;
    let (mut engine, handle) = Engine::new(
        Parts {
            recognizer: ServerRecognizer::new(config.server.clone()),
            wake,
            cues: CpalCuePlayer::new(config.cues, gate.clone()),
            gate,
            http,
            notice: Box::new(|notice| println!("{notice}")),
        },
        &config,
        recorder,
    );

    let mut states = handle.state();
    tokio::spawn(async move {
        loop {
            println!("{}", describe(&states.borrow_and_update()));
            if states.changed().await.is_err() {
                break;
            }
        }
    });
    println!("This build prints what it hears and delivers nothing. Ctrl+C stops it.");

    let mut interrupt = pin!(tokio::signal::ctrl_c());
    let result: Result<(), String> = Box::pin(async {
        loop {
            let trigger = tokio::select! {
                trigger = engine.listen(&mut frames) => trigger.map_err(|error| error.to_string())?,
                _ = &mut interrupt => return Ok(()),
            };
            match trigger {
                WakeTrigger::Conversation => {
                    println!("Conversation wake heard. This build has no conversation.");
                }
                WakeTrigger::Capture => {
                    let mut capture = pin!(engine.capture(&mut frames));
                    let (outcome, interrupted) = tokio::select! {
                        outcome = &mut capture => (outcome, false),
                        _ = &mut interrupt => {
                            handle.cancel();
                            ((&mut capture).await, true)
                        }
                    };
                    report(&outcome);
                    if interrupted {
                        return Ok(());
                    }
                }
            }
        }
    })
    .await;
    engine.finish().await;
    drop(microphone);
    println!("Stopped.");
    result
}
