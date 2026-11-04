//! In-process event bus and run-log hub.
//!
//! Every state change is published once; SSE subscribers, long-polling
//! clients and the scheduler all read from the same stream. Logs are appended
//! to one file per run and fanned out to followers with sequence numbers, so a
//! follower can catch up from disk and then switch to live lines without gaps
//! or duplicates.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tokio::sync::broadcast;

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    pub kind: &'static str,
    pub stack_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    pub at: String,
}

#[derive(Debug, Clone)]
pub enum LogMsg {
    Line(u64, Arc<str>),
    /// The current phase ended; followers should stop (the run is resting).
    End,
}

struct RunLog {
    file: std::fs::File,
    lines: u64,
    tx: broadcast::Sender<LogMsg>,
}

pub struct Hub {
    pub events: broadcast::Sender<Event>,
    logs: Mutex<HashMap<String, RunLog>>,
    log_dir: PathBuf,
}

pub enum Follow {
    /// Lines `[offset, upto)` are on disk; `rx` delivers everything after.
    Live { upto: u64, rx: broadcast::Receiver<LogMsg> },
    /// Nothing is being written right now; the file is complete.
    Done,
}

impl Hub {
    pub fn new(log_dir: PathBuf) -> Self {
        let (events, _) = broadcast::channel(1024);
        Hub { events, logs: Mutex::new(HashMap::new()), log_dir }
    }

    pub fn publish(&self, e: Event) {
        let _ = self.events.send(e);
    }

    pub fn log_path(&self, run_id: &str) -> PathBuf {
        self.log_dir.join(format!("{run_id}.log"))
    }

    /// Start (or resume) writing a run's log; announces it with a `run.log` event.
    pub fn open_log(&self, stack_id: &str, run_id: &str) -> std::io::Result<()> {
        let path = self.log_path(run_id);
        let existing = std::fs::read(&path).map(|b| bytecount_lines(&b)).unwrap_or(0);
        let file = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
        let (tx, _) = broadcast::channel(4096);
        self.logs
            .lock()
            .unwrap()
            .insert(run_id.to_string(), RunLog { file, lines: existing, tx });
        self.publish(Event {
            kind: "run.log",
            stack_id: stack_id.to_string(),
            run_id: Some(run_id.to_string()),
            status: None,
            at: crate::util::now(),
        });
        Ok(())
    }

    pub fn write_line(&self, run_id: &str, line: &str) {
        let mut logs = self.logs.lock().unwrap();
        if let Some(l) = logs.get_mut(run_id) {
            let _ = l.file.write_all(line.as_bytes());
            let _ = l.file.write_all(b"\n");
            let seq = l.lines;
            l.lines += 1;
            let _ = l.tx.send(LogMsg::Line(seq, Arc::from(line)));
        }
    }

    pub fn close_log(&self, run_id: &str) {
        if let Some(l) = self.logs.lock().unwrap().remove(run_id) {
            let _ = l.tx.send(LogMsg::End);
        }
    }

    pub fn follow(&self, run_id: &str) -> Follow {
        match self.logs.lock().unwrap().get(run_id) {
            Some(l) => Follow::Live { upto: l.lines, rx: l.tx.subscribe() },
            None => Follow::Done,
        }
    }
}

fn bytecount_lines(b: &[u8]) -> u64 {
    b.iter().filter(|&&c| c == b'\n').count() as u64
}
