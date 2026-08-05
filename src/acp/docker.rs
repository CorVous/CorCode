//! The transport that matters: the adapter run by `docker exec -i` inside a
//! chat's own container, speaking JSON-RPC over its stdio.

use std::pin::Pin;

use bollard::Docker;
use bollard::container::LogOutput;
use bollard::errors::Error as DockerError;
use bollard::exec::StartExecResults;
use bollard::models::ExecConfig;
use futures_util::{Stream, StreamExt as _};
use log::debug;
use tokio::io::{AsyncWrite, AsyncWriteExt as _};

use super::{AcpChannel, AcpError, AcpTransport};

/// The adapter binary baked into the workspace image (ADR-0004).
pub const ADAPTER: &str = "claude-agent-acp";

/// Starts adapters on the local Docker daemon.
pub struct DockerExec {
    docker: Docker,
}

impl DockerExec {
    /// Reach the daemon over the local socket.
    pub fn connect() -> Result<Self, AcpError> {
        Docker::connect_with_local_defaults()
            .map(|docker| Self { docker })
            .map_err(|source| AcpError::Unreachable {
                container: "the local docker daemon".to_owned(),
                source,
            })
    }
}

impl AcpTransport for DockerExec {
    type Channel = ExecChannel;

    async fn open(&self, container: &str) -> Result<Self::Channel, AcpError> {
        let unreachable = |source| AcpError::Unreachable {
            container: container.to_owned(),
            source,
        };
        let exec = self
            .docker
            .create_exec(
                container,
                ExecConfig {
                    attach_stdin: Some(true),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    tty: Some(false),
                    cmd: Some(vec![ADAPTER.to_owned()]),
                    ..ExecConfig::default()
                },
            )
            .await
            .map_err(unreachable)?;
        match self.docker.start_exec(&exec.id, None).await {
            Ok(StartExecResults::Attached { output, input }) => Ok(ExecChannel {
                output,
                input,
                unread: String::new(),
            }),
            Ok(StartExecResults::Detached) => Err(AcpError::Closed),
            Err(source) => Err(unreachable(source)),
        }
    }
}

/// One adapter's stdio, cut into newline-delimited messages.
pub struct ExecChannel {
    output: Pin<Box<dyn Stream<Item = Result<LogOutput, DockerError>> + Send>>,
    input: Pin<Box<dyn AsyncWrite + Send>>,
    unread: String,
}

impl AcpChannel for ExecChannel {
    async fn send(&mut self, message: &str) -> Result<(), AcpError> {
        let broken = |source| AcpError::Broken {
            doing: "writing to the adapter".to_owned(),
            source,
        };
        self.input
            .write_all(format!("{message}\n").as_bytes())
            .await
            .map_err(broken)?;
        self.input.flush().await.map_err(broken)
    }

    async fn receive(&mut self) -> Result<String, AcpError> {
        loop {
            if let Some(message) = take_line(&mut self.unread) {
                return Ok(message);
            }
            match self.output.next().await {
                Some(Ok(LogOutput::StdErr { message })) => {
                    debug!(
                        "adapter stderr: {}",
                        String::from_utf8_lossy(&message).trim()
                    );
                }
                Some(Ok(chunk)) => self.unread.push_str(&chunk.to_string()),
                Some(Err(source)) => {
                    return Err(AcpError::Unreachable {
                        container: "an attached adapter".to_owned(),
                        source,
                    });
                }
                None => return Err(AcpError::Closed),
            }
        }
    }
}

/// The first whole line waiting in `unread`, taken out of it.
fn take_line(unread: &mut String) -> Option<String> {
    let end = unread.find('\n')?;
    let line = unread[..end].trim().to_owned();
    unread.drain(..=end);
    Some(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_come_out_of_the_stream_a_line_at_a_time() {
        let mut unread = String::from("{\"id\":1}\n{\"id\"");

        assert_eq!(take_line(&mut unread).as_deref(), Some("{\"id\":1}"));
        assert_eq!(take_line(&mut unread), None, "half a line is no line");
        unread.push_str(":2}\n");
        assert_eq!(take_line(&mut unread).as_deref(), Some("{\"id\":2}"));
        assert!(unread.is_empty());
    }
}
