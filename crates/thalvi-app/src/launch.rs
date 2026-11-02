//! What a launch of the app asks for: a config path at the first start, an action on a relaunch.

use std::path::PathBuf;

/// What a relaunch makes the running app do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Open the settings window.
    Show,
    /// Pause listening, or resume it.
    Toggle,
    /// Cancel the capture in progress.
    Cancel,
}

/// The command line after the program name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Launch {
    pub command: Command,
    /// A config file to use instead of the per-user one.
    pub config: Option<PathBuf>,
}

impl Launch {
    /// # Errors
    /// The usage line when there is more than one argument.
    pub fn parse(args: &[String]) -> Result<Self, String> {
        match args {
            [] => Ok(Self {
                command: Command::Show,
                config: None,
            }),
            [word] if word == "toggle" => Ok(Self {
                command: Command::Toggle,
                config: None,
            }),
            [word] if word == "cancel" => Ok(Self {
                command: Command::Cancel,
                config: None,
            }),
            [path] => Ok(Self {
                command: Command::Show,
                config: Some(PathBuf::from(path)),
            }),
            _ => Err("Usage: thalvi-desktop [toggle | cancel | config-file]".to_owned()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Launch, String> {
        Launch::parse(&args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>())
    }

    #[test]
    fn a_launch_names_a_command_or_a_config_file() {
        let launch = |command, config: Option<&str>| {
            Ok(Launch {
                command,
                config: config.map(PathBuf::from),
            })
        };
        for (args, expected) in [
            (&[][..], launch(Command::Show, None)),
            (&["toggle"][..], launch(Command::Toggle, None)),
            (&["cancel"][..], launch(Command::Cancel, None)),
            (
                &["desktop.json"][..],
                launch(Command::Show, Some("desktop.json")),
            ),
            (
                &["a", "b"][..],
                Err("Usage: thalvi-desktop [toggle | cancel | config-file]".to_owned()),
            ),
        ] {
            assert_eq!(parse(args), expected, "{args:?}");
        }
    }
}
