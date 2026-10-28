//! Running external commands so `--debug` shows each one under the block
//! that ran it, with how long it took.

use std::io;
use std::process::{Child, Command, ExitStatus, Output};
use std::time::{Duration, Instant};

/// [`Command`] methods that log the command to the current block's debug
/// output. See [`crate::output::BlockScope`].
pub trait CommandExt {
    /// [`Command::output`], logged with its exit status and duration.
    fn logged_output(&mut self) -> io::Result<Output>;
    /// [`Command::status`], logged with its exit status and duration.
    fn logged_status(&mut self) -> io::Result<ExitStatus>;
    /// [`Command::spawn`], logged when it starts. The caller waits, so the
    /// duration shows as the gap to the block's next message.
    fn logged_spawn(&mut self) -> io::Result<Child>;
}

impl CommandExt for Command {
    fn logged_output(&mut self) -> io::Result<Output> {
        let start = Instant::now();
        let output = self.output();
        log_finished(self, start.elapsed(), output.as_ref().map(|output| output.status));
        output
    }

    fn logged_status(&mut self) -> io::Result<ExitStatus> {
        let start = Instant::now();
        let status = self.status();
        log_finished(self, start.elapsed(), status.as_ref().map(|status| *status));
        status
    }

    fn logged_spawn(&mut self) -> io::Result<Child> {
        crate::output::debug_current(|| format!("exec: {}", describe(self)));
        self.spawn()
    }
}

fn log_finished(command: &Command, elapsed: Duration, status: Result<ExitStatus, &io::Error>) {
    crate::output::debug_current(|| {
        let result = match status {
            Ok(status) if status.success() => "ok".to_owned(),
            Ok(status) => status.to_string(),
            Err(error) => format!("failed to run: {error}"),
        };
        format!("exec: {} ({result}, {}ms)", describe(command), elapsed.as_millis())
    });
}

/// The command line, with its working directory and any variables it sets.
fn describe(command: &Command) -> String {
    let mut parts: Vec<String> = command
        .get_envs()
        .filter_map(|(name, value)| Some(format!("{}={}", name.to_string_lossy(), value?.to_string_lossy())))
        .collect();
    parts.push(command.get_program().to_string_lossy().into_owned());
    parts.extend(command.get_args().map(|arg| arg.to_string_lossy().into_owned()));
    // In full, unlike the elided commands start lines show.
    let line = parts.join(" ");
    match command.get_current_dir() {
        Some(dir) => format!("{line} (in {})", dir.display()),
        None => line,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_the_full_command_with_its_variables_and_directory() {
        let mut command = Command::new("go");
        command
            .args(["list", "-m", "-json"])
            .env("GOWORK", "off")
            .current_dir("svc");
        assert_eq!(describe(&command), "GOWORK=off go list -m -json (in svc)");
        assert_eq!(describe(&Command::new("true")), "true");
    }
}
