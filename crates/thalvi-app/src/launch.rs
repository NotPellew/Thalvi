//! What a launch of the app asks for: a config path at the first start, an action on a relaunch.

use std::path::PathBuf;

use crate::startup::LOGIN_ARG;

const USAGE: &str = "Usage: thalvi-desktop [toggle | pause | resume | cancel | hold-start | hold-end | write-start | write-end | copy-error | status | config-file]";

/// What a relaunch makes the running app do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Open the settings window.
    Show,
    /// What the login entry asks for: start in the tray, and leave a running app as it is.
    Login,
    /// Pause listening, or resume it.
    Toggle,
    /// Close the microphone and drop any capture in progress.
    Pause,
    /// Open the microphone again; the wake word still starts a capture.
    Resume,
    /// Cancel the capture in progress.
    Cancel,
    /// The dictation key went down: start a capture that its release ends.
    HoldStart,
    /// The dictation key came up.
    HoldEnd,
    /// The write key went down: start a capture that its release writes out and pastes.
    WriteStart,
    /// The write key came up.
    WriteEnd,
    /// Copy the report of the last error shown, without opening a window.
    CopyError,
}

impl Command {
    /// # Errors
    /// Why this command may not start the app: only opening it may, the others control a running one.
    pub fn first_start(self) -> Result<(), String> {
        match self {
            Self::Show | Self::Login => Ok(()),
            _ => Err(
                "Thalvi is not running, so there is nothing to control. Start Thalvi first."
                    .to_owned(),
            ),
        }
    }
}

/// What a second launch, with its full command line, asks the running app to do.
///
/// A line that cannot be read still opens the settings.
#[must_use]
pub fn relaunch_command(args: &[String]) -> Command {
    match Launch::parse(args.get(1..).unwrap_or_default()) {
        Ok(Launch::Run { command, .. }) => command,
        _ => Command::Show,
    }
}

/// The command line after the program name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Launch {
    /// Print the running app's state for a status bar; never starts the app.
    Status,
    Run {
        command: Command,
        /// A config file to use instead of the per-user one.
        config: Option<PathBuf>,
    },
}

impl Launch {
    /// # Errors
    /// The usage line when there is more than one argument.
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let run = |command, config| Ok(Self::Run { command, config });
        match args {
            [] => run(Command::Show, None),
            [word] => match word.as_str() {
                "status" => Ok(Self::Status),
                LOGIN_ARG => run(Command::Login, None),
                "toggle" => run(Command::Toggle, None),
                "pause" => run(Command::Pause, None),
                "resume" => run(Command::Resume, None),
                "cancel" => run(Command::Cancel, None),
                "hold-start" => run(Command::HoldStart, None),
                "hold-end" => run(Command::HoldEnd, None),
                "write-start" => run(Command::WriteStart, None),
                "write-end" => run(Command::WriteEnd, None),
                "copy-error" => run(Command::CopyError, None),
                path => run(Command::Show, Some(PathBuf::from(path))),
            },
            _ => Err(USAGE.to_owned()),
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
        let run = |command, config: Option<&str>| {
            Ok(Launch::Run {
                command,
                config: config.map(PathBuf::from),
            })
        };
        for (args, expected) in [
            (&[][..], run(Command::Show, None)),
            (&["toggle"][..], run(Command::Toggle, None)),
            (&["pause"][..], run(Command::Pause, None)),
            (&["resume"][..], run(Command::Resume, None)),
            (&["cancel"][..], run(Command::Cancel, None)),
            (&["hold-start"][..], run(Command::HoldStart, None)),
            (&["hold-end"][..], run(Command::HoldEnd, None)),
            (&["write-start"][..], run(Command::WriteStart, None)),
            (&["write-end"][..], run(Command::WriteEnd, None)),
            (&["copy-error"][..], run(Command::CopyError, None)),
            (&["status"][..], Ok(Launch::Status)),
            (&["--login"][..], run(Command::Login, None)),
            (
                &["desktop.json"][..],
                run(Command::Show, Some("desktop.json")),
            ),
            (&["a", "b"][..], Err(USAGE.to_owned())),
            (&["toggle", "x"][..], Err(USAGE.to_owned())),
        ] {
            assert_eq!(parse(args), expected, "{args:?}");
        }
    }

    #[test]
    fn a_relaunch_from_the_menu_opens_settings_but_one_from_login_does_not() {
        let relaunch = |args: &[&str]| {
            relaunch_command(&args.iter().map(|arg| (*arg).to_owned()).collect::<Vec<_>>())
        };
        for (args, expected) in [
            (&["thalvi-desktop"][..], Command::Show),
            (&["thalvi-desktop", "--login"][..], Command::Login),
            (&["thalvi-desktop", "toggle"][..], Command::Toggle),
            (&["thalvi-desktop", "hold-start"][..], Command::HoldStart),
            (&["thalvi-desktop", "hold-end"][..], Command::HoldEnd),
            (&["thalvi-desktop", "write-start"][..], Command::WriteStart),
            (&["thalvi-desktop", "write-end"][..], Command::WriteEnd),
            (&["thalvi-desktop", "a", "b"][..], Command::Show),
            (&[][..], Command::Show),
        ] {
            assert_eq!(relaunch(args), expected, "{args:?}");
        }
    }

    #[test]
    fn only_opening_the_app_may_start_it() {
        for (command, starts) in [
            (Command::Show, true),
            (Command::Login, true),
            (Command::Toggle, false),
            (Command::Pause, false),
            (Command::Resume, false),
            (Command::Cancel, false),
            (Command::HoldStart, false),
            (Command::HoldEnd, false),
            (Command::WriteStart, false),
            (Command::WriteEnd, false),
            (Command::CopyError, false),
        ] {
            assert_eq!(command.first_start().is_ok(), starts, "{command:?}");
        }
    }
}
