//! Thalvi desktop app: wake, capture, cues and delivery into the focused window.

pub mod audio;
pub mod boosting;
pub mod capture;
pub mod chat;
pub mod chosen;
pub mod claim;
pub mod config;
pub mod cues;
pub mod delivery;
pub mod device;
pub mod draft;
pub mod engine;
pub mod entries;
pub mod fbank;
pub mod feed;
pub mod frame;
pub mod history;
pub mod hold;
pub mod host;
pub mod local;
pub mod merge;
pub mod metrics;
pub mod models;
pub mod numbers;
pub mod openai_live;
pub mod phrases;
pub mod private;
pub mod realtime;
pub mod recognizer;
pub mod report;
pub mod runtime;
pub mod secondary;
pub mod segments;
pub mod speech_device;
pub mod spotter;
pub mod tasks;
mod tdt;
pub mod theme;
pub mod transducer;
pub mod vad;
pub mod wake;
pub mod wake_stats;
pub mod whisper;
pub mod writing;

// Temporary issue 693 feature fixture.
