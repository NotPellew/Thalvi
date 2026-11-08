//! Meta's realtime ASR protocol, spoken by Meta and by `speech/asr`; the server and the desktop both connect with it.

mod client;

pub use client::{Connection, Error, Event, Handshake, parse_event};
