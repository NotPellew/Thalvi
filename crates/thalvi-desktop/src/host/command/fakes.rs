//! A command runner double that records every command and answers with a closure.

use std::sync::Mutex;

use super::{CommandError, CommandRunner};

type Reply = Box<dyn Fn(&[String], Option<&str>) -> Result<String, CommandError> + Send + Sync>;

/// Records every command and answers with `reply`.
pub struct FakeRunner {
    pub calls: Mutex<Vec<(Vec<String>, Option<String>)>>,
    pub reply: Reply,
}

impl FakeRunner {
    pub fn new(
        reply: impl Fn(&[String], Option<&str>) -> Result<String, CommandError> + Send + Sync + 'static,
    ) -> Self {
        Self {
            calls: Mutex::default(),
            reply: Box::new(reply),
        }
    }

    /// Every command so far as one line, with its stdin in angle brackets.
    pub fn commands(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|(argv, stdin)| match stdin {
                Some(stdin) => format!("{} <{stdin}>", argv.join(" ")),
                None => argv.join(" "),
            })
            .collect()
    }
}

impl CommandRunner for FakeRunner {
    fn run(
        &self,
        program: &str,
        args: &[&str],
        stdin: Option<&str>,
    ) -> Result<String, CommandError> {
        let command: Vec<String> = std::iter::once(program)
            .chain(args.iter().copied())
            .map(str::to_owned)
            .collect();
        let reply = (self.reply)(&command, stdin);
        self.calls
            .lock()
            .unwrap()
            .push((command, stdin.map(str::to_owned)));
        reply
    }
}
