//! A signer's `exec` need (`Need::Exec`): the command it names, run by
//! the host under the operator's grant as kubectl runs a credential
//! plugin: its argv, never a shell; the host's environment with the
//! command's variables set over it; nothing on its standard input; its
//! standard output the signer's; each line of its standard error an
//! event of the host's log (`DFORM_LOG=info`), its last the failure's
//! when it fails; stopped past its timeout ([`TIMEOUT`]).

use dform_core::plugin::signer::Command;
use std::io::{BufRead, BufReader, Read};
use std::process::{Child, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

/// How long a signer's command may run: a credential plugin answers from
/// its cache or a token endpoint (`gke-gcloud-auth-plugin`, `aws eks
/// get-token`), and is run not interactively, so it waits on no one.
pub const TIMEOUT: Duration = Duration::from_secs(60);

/// A command running: its program, for messages, and what its standard
/// output and error come to.
pub struct Running {
    program: String,
    child: Child,
    stdout: Receiver<Vec<u8>>,
    /// Its standard error's last line, once it closes.
    last: Receiver<Option<String>>,
}

impl Running {
    /// `c` started.
    pub fn start(c: &Command) -> Result<Running, String> {
        let (program, args) = c
            .argv
            .split_first()
            .ok_or_else(|| "its signer asks to run a command that names no program".to_string())?;
        let mut child = std::process::Command::new(program)
            .args(args)
            .envs(c.env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("its signer's command {program} does not start: {e}"))?;
        let stdout = read(child.stdout.take());
        let last = logged(program, child.stderr.take());
        Ok(Running {
            program: program.clone(),
            child,
            stdout,
            last,
        })
    }

    /// Its standard output, once it exits with success within `timeout`;
    /// else why not, naming it.
    pub fn output(mut self, timeout: Duration) -> Result<Vec<u8>, String> {
        let deadline = Instant::now() + timeout;
        let Some(status) = self.exit(deadline)? else {
            let _ = self.child.kill();
            let _ = self.child.wait();
            return Err(format!(
                "its signer's command {} did not finish in {}; it was stopped",
                self.program,
                dform_core::plugin::policy::show(timeout)
            ));
        };
        if !status.success() {
            let why = match self.last.recv_timeout(left(deadline)) {
                Ok(Some(line)) => format!(": {line}"),
                _ => String::new(),
            };
            return Err(format!(
                "its signer's command {} failed ({status}){why}",
                self.program
            ));
        }
        // What it left holding the pipe is waited for only until then.
        self.stdout.recv_timeout(left(deadline)).map_err(|_| {
            format!(
                "its signer's command {} exited, and what it started holds its output",
                self.program
            )
        })
    }

    /// Its status, once it exits before `deadline`.
    fn exit(&mut self, deadline: Instant) -> Result<Option<ExitStatus>, String> {
        loop {
            let polled = self.child.try_wait().map_err(|e| {
                format!(
                    "its signer's command {} is not waited for: {e}",
                    self.program
                )
            })?;
            if polled.is_some() || Instant::now() >= deadline {
                return Ok(polled);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// The time left until `deadline`.
fn left(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

/// All of `pipe`, read on a thread of its own, once it closes.
fn read(pipe: Option<impl Read + Send + 'static>) -> Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        if let Some(mut pipe) = pipe {
            let _ = pipe.read_to_end(&mut out);
        }
        let _ = tx.send(out);
    });
    rx
}

/// Each line of `pipe` an event of the log, read on a thread of its own;
/// its last line that says something, once it closes.
fn logged(program: &str, pipe: Option<impl Read + Send + 'static>) -> Receiver<Option<String>> {
    let (tx, rx) = mpsc::channel();
    let program = program.to_string();
    std::thread::spawn(move || {
        let mut last = None;
        for line in pipe.into_iter().flat_map(|p| BufReader::new(p).lines()) {
            let Ok(line) = line else { break };
            tracing::info!(target: "dform", program = %program, "{line}");
            if !line.trim().is_empty() {
                last = Some(line.trim().to_string());
            }
        }
        let _ = tx.send(last);
    });
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str, env: &[(&str, &str)]) -> Command {
        Command {
            argv: ["sh", "-c", script].map(String::from).to_vec(),
            env: env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    fn output(c: &Command, timeout: Duration) -> Result<Vec<u8>, String> {
        Running::start(c)?.output(timeout)
    }

    /// Its standard output, with the variables it was given, and nothing
    /// on its standard input.
    #[test]
    fn its_output_is_the_signers() {
        let c = sh("cat; printf '%s' \"$T\"; echo oops >&2", &[("T", "tok")]);
        assert_eq!(output(&c, TIMEOUT).unwrap(), b"tok");
    }

    /// A failure names the program, its status and the last line it said;
    /// one past its timeout is stopped; one that is not there is named.
    #[test]
    fn a_failure_says_why() {
        let e = output(&sh("echo 'log in first' >&2; exit 3", &[]), TIMEOUT).unwrap_err();
        assert_eq!(
            e,
            "its signer's command sh failed (exit status: 3): log in first"
        );
        let started = Instant::now();
        let e = output(&sh("sleep 5", &[]), Duration::from_millis(100)).unwrap_err();
        assert_eq!(
            e,
            "its signer's command sh did not finish in 100ms; it was stopped"
        );
        assert!(started.elapsed() < Duration::from_secs(4));
        let none = Command {
            argv: vec!["/nonexistent/plugin".into()],
            env: Vec::new(),
        };
        let e = output(&none, TIMEOUT).unwrap_err();
        assert!(
            e.starts_with("its signer's command /nonexistent/plugin does not start"),
            "{e}"
        );
    }
}
