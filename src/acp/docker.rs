//! The transport that matters: the adapter run by `docker exec -i` inside a
//! chat's own container, speaking JSON-RPC over its stdio.

use std::pin::Pin;
use std::string::FromUtf8Error;

use bollard::Docker;
use bollard::container::LogOutput;
use bollard::errors::Error as DockerError;
use bollard::exec::StartExecResults;
use bollard::models::ExecConfig;
use futures_util::{Stream, StreamExt as _};
use log::{info, warn};
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
                unread: Vec::new(),
            }),
            Ok(StartExecResults::Detached) => Err(AcpError::Closed),
            Err(source) => Err(unreachable(source)),
        }
    }
}

/// One adapter's stdio, cut into newline-delimited messages. The bytes are
/// held as bytes until a whole line is there: the daemon breaks the pipe
/// wherever it likes, and a character broken in half is not text yet.
pub struct ExecChannel {
    output: Pin<Box<dyn Stream<Item = Result<LogOutput, DockerError>> + Send>>,
    input: Pin<Box<dyn AsyncWrite + Send>>,
    unread: Vec<u8>,
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
            if let Some(line) = take_line(&mut self.unread) {
                match String::from_utf8(line) {
                    Ok(message) => return Ok(message.trim().to_owned()),
                    Err(nonsense) => warn!("{}", undecodable(&nonsense)),
                }
                continue;
            }
            match self.output.next().await {
                Some(Ok(LogOutput::StdErr { message })) => {
                    info!(
                        "adapter stderr: {}",
                        String::from_utf8_lossy(&message).trim()
                    );
                }
                Some(Ok(chunk)) => self.unread.extend_from_slice(chunk.as_ref()),
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

/// The line the operator reads when the adapter's stream carries bytes that
/// are not text: where in the line they went wrong, and the line itself with
/// those bytes stood in for (issue #66).
fn undecodable(nonsense: &FromUtf8Error) -> String {
    format!(
        "the adapter's stream carried a line that is not utf-8 ({nonsense}): {}",
        String::from_utf8_lossy(nonsense.as_bytes())
    )
}

/// The first whole line waiting in `unread`, taken out of it.
fn take_line(unread: &mut Vec<u8>) -> Option<Vec<u8>> {
    let end = unread.iter().position(|byte| *byte == b'\n')?;
    let line = unread[..end].to_vec();
    unread.drain(..=end);
    Some(line)
}

#[cfg(test)]
mod tests {
    use log::Level;

    use super::*;
    use crate::logs::capturing_lines;

    #[test]
    fn a_line_that_is_not_text_is_logged_with_where_the_bytes_went_wrong() {
        let nonsense =
            String::from_utf8(b"{\"id\":\xff}".to_vec()).expect_err("those bytes are not utf-8");

        let logged = undecodable(&nonsense);

        assert_eq!(
            logged,
            "the adapter's stream carried a line that is not utf-8 \
             (invalid utf-8 sequence of 1 bytes from index 6): {\"id\":\u{fffd}}",
            "a parse failure without the line is a parse failure nobody can diagnose"
        );
    }

    /// The line is only worth writing if a deployment reads it, and a
    /// deployment reads warnings (issue #84).
    #[tokio::test]
    async fn a_line_that_is_not_text_reaches_the_operator_as_a_warning() {
        let logged = capturing_lines();
        let mut channel = channel_reading(&[b"{\"id\":\xff}\n"]);

        channel.receive().await.expect_err("that stream ends");

        assert_eq!(
            logged.quietest_level_of("carried a line that is not utf-8"),
            Some(Level::Warn),
            "src/acp/docker.rs: an undecodable line below WARN is one the deployed core never says"
        );
    }

    /// The adapter talking about itself is healthy chatter: worth having under
    /// `RUST_LOG=info`, not worth warning a deployment about (issue #84).
    #[tokio::test]
    async fn the_adapters_own_stderr_is_forwarded_as_chatter_rather_than_a_warning() {
        let logged = capturing_lines();
        let mut channel = channel_hearing(b"npm notice: a new version is out");

        channel.receive().await.expect_err("that stream ends");

        assert_eq!(
            logged.quietest_level_of("adapter stderr: npm notice"),
            Some(Level::Info),
            "src/acp/docker.rs: the adapter's stderr is forwarded at INFO"
        );
    }

    #[test]
    fn messages_come_out_of_the_stream_a_line_at_a_time() {
        let mut unread = Vec::from(*b"{\"id\":1}\n{\"id\"");

        assert_eq!(take_line(&mut unread).as_deref(), Some(&b"{\"id\":1}"[..]));
        assert_eq!(take_line(&mut unread), None, "half a line is no line");
        unread.extend_from_slice(b":2}\n");
        assert_eq!(take_line(&mut unread).as_deref(), Some(&b"{\"id\":2}"[..]));
        assert!(unread.is_empty());
    }

    #[tokio::test]
    async fn a_line_that_is_not_text_arrives_with_its_bad_bytes_stood_in_for() {
        let mut channel = channel_reading(&[b"{\"id\":\xff}\n"]);

        let arrived = channel.receive().await.expect("the line should arrive");

        assert_eq!(
            arrived, "{\"id\":\u{fffd}}",
            "a line dropped here never reaches the parse that calls the stream \
             garbled (issue #65)"
        );
    }

    #[tokio::test]
    async fn a_message_cut_through_a_character_arrives_whole() {
        let message = r#"{"jsonrpc":"2.0","id":1,"result":{"sessionId":"café"}}"#;
        let framed = format!("{message}\n").into_bytes();
        let cut = message
            .find('é')
            .expect("the message spells a multi-byte character")
            + 1;
        let mut channel = channel_reading(&[&framed[..cut], &framed[cut..]]);

        let arrived = channel.receive().await.expect("the message should arrive");

        assert_eq!(arrived, message);
    }

    /// A channel over a stream that hands out exactly these chunks, as the
    /// daemon hands out whatever happened to be in the pipe.
    fn channel_reading(chunks: &[&[u8]]) -> ExecChannel {
        channel_over(
            chunks
                .iter()
                .map(|chunk| LogOutput::StdOut {
                    message: chunk.to_vec().into(),
                })
                .collect(),
        )
    }

    /// A channel over a stream that carries one line of the adapter's own
    /// stderr, which is what the daemon hands over when the adapter talks
    /// about itself rather than to us.
    fn channel_hearing(stderr: &[u8]) -> ExecChannel {
        channel_over(vec![LogOutput::StdErr {
            message: stderr.to_vec().into(),
        }])
    }

    fn channel_over(output: Vec<LogOutput>) -> ExecChannel {
        ExecChannel {
            output: Box::pin(futures_util::stream::iter(output.into_iter().map(Ok))),
            input: Box::pin(tokio::io::sink()),
            unread: Vec::new(),
        }
    }
}
