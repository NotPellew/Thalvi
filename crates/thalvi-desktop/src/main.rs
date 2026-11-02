//! Runs the capture engine on the default microphone and delivers what it hears to the desktop.

use std::path::PathBuf;
use std::pin::pin;
use std::process::ExitCode;
use std::time::Instant;

use serde_json::Value;
use thalvi_desktop::audio::{InputGate, Microphone};
use thalvi_desktop::config::DesktopConfig;
use thalvi_desktop::delivery::{self, Delivery};
use thalvi_desktop::engine::{
    CaptureOutcome, CpalCuePlayer, Engine, EngineState, Parts, WakeTrigger,
};
use thalvi_desktop::host::Desktop;
use thalvi_desktop::metrics::{Event, Recorder};
use thalvi_desktop::models::ModelStore;
use thalvi_desktop::recognizer::ServerRecognizer;
use thalvi_desktop::wake::WakeEngine;
use tokio::sync::watch;
use tokio::task::LocalSet;

fn main() -> ExitCode {
    // Recording failures and dropped events are reported only through `tracing`.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();
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
    let host = Desktop::detect().map_err(|error| error.to_string())?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("Thalvi could not start: {error}"))?;
    LocalSet::new().block_on(&runtime, listen_and_deliver(config, host))
}

fn describe(state: &EngineState, window: Option<&str>) -> String {
    if let Some(notice) = window.and_then(|window| delivery::submit_notice(state, window)) {
        return notice;
    }
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
        // Delivery's notice says what failed and where the dictated text went.
        CaptureOutcome::Failed { .. } => {}
    }
}

async fn listen_and_deliver(config: DesktopConfig, host: Desktop) -> Result<(), String> {
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

    let delivery = Delivery::new(host);
    let (window_tx, window) = watch::channel(None::<String>);
    let mut states = handle.state();
    tokio::spawn(async move {
        loop {
            println!(
                "{}",
                describe(&states.borrow_and_update(), window.borrow().as_deref())
            );
            if states.changed().await.is_err() {
                break;
            }
        }
    });
    println!("Transcribing through {}.", config.server);
    println!("Ctrl+C stops it.");

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
                    let bound = delivery.bind(engine.recorder()).await;
                    window_tx.send_replace(bound.window());
                    let (outcome, interrupted) = {
                        let mut capture = pin!(engine.capture(&mut frames));
                        tokio::select! {
                            outcome = &mut capture => (outcome, false),
                            _ = &mut interrupt => {
                                handle.cancel();
                                ((&mut capture).await, true)
                            }
                        }
                    };
                    window_tx.send_replace(None);
                    report(&outcome);
                    if interrupted {
                        return Ok(());
                    }
                    if let Some(notice) = delivery
                        .finish(&outcome, &bound, &config, engine.recorder())
                        .await
                    {
                        println!("{notice}");
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
