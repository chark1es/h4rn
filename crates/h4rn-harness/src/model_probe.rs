//! Short-lived CLI discovery without sending a prompt.

use std::io;
use std::process::Stdio;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

pub(crate) struct ModelProbe {
    _child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
}

impl ModelProbe {
    pub(crate) fn spawn(command: std::process::Command) -> io::Result<Self> {
        let mut child = Command::from(command)
            .kill_on_drop(true)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let stdin = child.stdin.take().expect("piped stdin");
        let lines = BufReader::new(child.stdout.take().expect("piped stdout")).lines();
        Ok(Self {
            _child: child,
            stdin,
            lines,
        })
    }

    pub(crate) async fn send(&mut self, frame: Value) -> io::Result<()> {
        let mut line = serde_json::to_vec(&frame)?;
        line.push(b'\n');
        self.stdin.write_all(&line).await
    }

    pub(crate) async fn receive(&mut self, matches: impl Fn(&Value) -> bool) -> io::Result<Value> {
        while let Some(line) = self.lines.next_line().await? {
            if let Ok(frame) = serde_json::from_str(&line)
                && matches(&frame)
            {
                return Ok(frame);
            }
        }
        Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "model probe exited",
        ))
    }
}

pub(crate) fn model_ids(rows: &Value, field: &str) -> Vec<String> {
    rows.as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| row[field].as_str().map(str::to_owned))
        .collect()
}
