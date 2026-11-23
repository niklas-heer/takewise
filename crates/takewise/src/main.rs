//! `takewise`: record narration over a film; the command line for agents.

mod calibrate;
mod cli;
mod cli_install;
mod film;
mod output;
mod window;

use std::ffi::OsString;
use std::process::ExitCode;

use clap::Parser as _;

/// The bundle identifier in `packaging/Info.plist`.
const BUNDLE_ID: &str = "com.niklasheer.takewise";

/// Whether Takewise.app was opened by itself (from the Dock or Finder),
/// without a command: no arguments but those macOS may add (`-psn_…`), and
/// launched as Takewise.app, as the environment macOS gives an app says.
/// A bare `takewise` in a terminal stays the command line.
fn opened_as_app(args: &[OsString], env: impl Fn(&str) -> Option<OsString>) -> bool {
    let no_command = args
        .iter()
        .all(|arg| arg.to_string_lossy().starts_with("-psn_"));
    let ours = |name: &str, matches: &dyn Fn(&str) -> bool| {
        env(name).is_some_and(|value| matches(&value.to_string_lossy()))
    };
    let launched = ours("__CFBundleIdentifier", &|v| v == BUNDLE_ID)
        || ours("XPC_SERVICE_NAME", &|v| {
            v.starts_with(&format!("application.{BUNDLE_ID}."))
        });
    no_command && launched
}

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    if opened_as_app(&args, |name| std::env::var_os(name)) {
        window::run_start();
        return ExitCode::SUCCESS;
    }
    let json = args.iter().any(|arg| arg == "--json");
    let args = match cli::Cli::try_parse() {
        Ok(args) => args,
        Err(err) if !err.use_stderr() => {
            // --help and --version
            let _ = err.print();
            return ExitCode::SUCCESS;
        }
        Err(err) if json => {
            output::print_error(true, "usage", &err.render().to_string());
            return ExitCode::from(2);
        }
        Err(err) => {
            let _ = err.print();
            return ExitCode::from(2);
        }
    };
    let json = args.json;
    match cli::run(args) {
        Ok(outcome) => outcome.print(json),
        Err(err) => {
            output::print_error(json, err.code(), &err.to_string());
            ExitCode::from(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name| {
            vars.iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| OsString::from(v))
        }
    }

    #[test]
    fn opened_from_the_dock_shows_the_start_window() {
        let app = [("__CFBundleIdentifier", BUNDLE_ID)];
        assert!(opened_as_app(&[], env(&app)));
        assert!(opened_as_app(&["-psn_0_12345".into()], env(&app)));
        let xpc = [(
            "XPC_SERVICE_NAME",
            "application.com.niklasheer.takewise.1234.5678",
        )];
        assert!(opened_as_app(&[], env(&xpc)));
    }

    #[test]
    fn the_command_line_stays_the_command_line() {
        let app = [("__CFBundleIdentifier", BUNDLE_ID)];
        // A command, even inside the app (`record` opens it this way).
        assert!(!opened_as_app(&["window".into(), "x".into()], env(&app)));
        // A bare `takewise` in a terminal.
        let terminal = [
            ("__CFBundleIdentifier", "com.apple.Terminal"),
            ("XPC_SERVICE_NAME", "application.com.apple.Terminal.1.2"),
        ];
        assert!(!opened_as_app(&[], env(&terminal)));
        assert!(!opened_as_app(&[], env(&[])));
    }
}
