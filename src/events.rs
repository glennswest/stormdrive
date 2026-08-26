//! Event ring: what happened to which drive, when. The UI and any external
//! poller read it via `GET /api/v1/events?since=<seq>`.

use crate::drive::DriveId;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::time::SystemTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub seq: u64,
    pub time: SystemTime,
    pub drive_id: Option<DriveId>,
    pub severity: Severity,
    /// Machine-readable kind: discovered, missing, health, state, stormblock.
    pub kind: String,
    pub message: String,
}

#[derive(Debug)]
pub struct EventLog {
    next_seq: u64,
    ring: VecDeque<Event>,
    cap: usize,
}

impl EventLog {
    pub fn new(cap: usize) -> Self {
        Self {
            next_seq: 1,
            ring: VecDeque::with_capacity(cap.min(1024)),
            cap,
        }
    }

    pub fn push(
        &mut self,
        drive_id: Option<DriveId>,
        severity: Severity,
        kind: &str,
        message: String,
    ) -> u64 {
        let seq = self.next_seq;
        self.next_seq += 1;
        if self.ring.len() == self.cap {
            self.ring.pop_front();
        }
        self.ring.push_back(Event {
            seq,
            time: SystemTime::now(),
            drive_id,
            severity,
            kind: kind.to_string(),
            message,
        });
        seq
    }

    /// Events with seq strictly greater than `since`.
    pub fn since(&self, since: u64) -> Vec<Event> {
        self.ring.iter().filter(|e| e.seq > since).cloned().collect()
    }

    pub fn latest_seq(&self) -> u64 {
        self.next_seq - 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_caps_and_since_filters() {
        let mut log = EventLog::new(3);
        for i in 0..5 {
            log.push(None, Severity::Info, "test", format!("e{i}"));
        }
        assert_eq!(log.latest_seq(), 5);
        let all = log.since(0);
        assert_eq!(all.len(), 3, "ring keeps only cap entries");
        assert_eq!(all[0].seq, 3);
        assert_eq!(log.since(4).len(), 1);
        assert!(log.since(5).is_empty());
    }
}
