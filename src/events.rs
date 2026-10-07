//! Event ring: what happened to which drive, when. The UI and any external
//! poller read it via `GET /api/v1/events?since=<seq>`. The last
//! [`PERSIST_TAIL`] events and the next sequence number are kept in
//! `<data_dir>/events.json` (#25), so a restart keeps "safe to pull" and the
//! rest, and `seq` never goes backwards for a poller holding `since=N`.

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
    /// Machine-readable kind: discovered, missing, replaced, forgotten,
    /// location, health, designation, overcommit, fleet, drain, test,
    /// format, firmware, shelf, hba, stormblock, worker, restart (#39: a
    /// drive left busy by a run that did not survive), audit (#45: who
    /// made which write, and whether it was allowed), operation.
    pub kind: String,
    pub message: String,
}

/// How many of the newest events survive a restart.
pub const PERSIST_TAIL: usize = 512;

/// `events.json`.
#[derive(Debug, Serialize, Deserialize)]
struct Saved {
    next_seq: u64,
    events: Vec<Event>,
}

#[derive(Debug)]
pub struct EventLog {
    next_seq: u64,
    ring: VecDeque<Event>,
    cap: usize,
    /// When this process started its log: a client can tell a restart.
    started: SystemTime,
}

impl EventLog {
    pub fn new(cap: usize) -> Self {
        Self {
            next_seq: 1,
            ring: VecDeque::with_capacity(cap.min(1024)),
            cap,
            started: SystemTime::now(),
        }
    }

    /// The log a previous run saved (`events.json`): its events, and the
    /// sequence continuing after them. Unreadable or absent = a new log.
    pub fn restore(cap: usize, saved: Option<&[u8]>) -> Self {
        let mut log = Self::new(cap);
        let Some(s) = saved.and_then(|b| serde_json::from_slice::<Saved>(b).ok()) else {
            return log;
        };
        let skip = s.events.len().saturating_sub(cap);
        log.ring.extend(s.events.into_iter().skip(skip));
        let after_last = log.ring.back().map(|e| e.seq + 1).unwrap_or(1);
        log.next_seq = s.next_seq.max(after_last);
        log
    }

    /// What `events.json` holds: the newest `tail` events and the next seq.
    pub fn snapshot(&self, tail: usize) -> Vec<u8> {
        let skip = self.ring.len().saturating_sub(tail);
        let saved = Saved { next_seq: self.next_seq, events: self.ring.iter().skip(skip).cloned().collect() };
        serde_json::to_vec(&saved).unwrap_or_default()
    }

    pub fn len(&self) -> usize {
        self.ring.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }

    pub fn started(&self) -> SystemTime {
        self.started
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

    #[test]
    fn a_restart_keeps_the_tail_and_the_sequence() {
        let mut log = EventLog::new(4096);
        for i in 0..600 {
            log.push(None, Severity::Info, "test", format!("e{i}"));
        }
        let saved = log.snapshot(PERSIST_TAIL);
        let back = EventLog::restore(4096, Some(&saved));
        assert_eq!(back.len(), PERSIST_TAIL);
        assert_eq!(back.since(0)[0].seq, 600 - 512 + 1, "the newest 512");
        assert_eq!(back.latest_seq(), 600, "seq does not go backwards");
        let mut back = back;
        assert_eq!(back.push(None, Severity::Info, "restart", "up".into()), 601);
        // A smaller ring keeps the newest of what was saved.
        assert_eq!(EventLog::restore(10, Some(&saved)).since(0)[0].seq, 591);
        // Nothing saved, or garbage: a new log at seq 1.
        assert_eq!(EventLog::restore(10, None).latest_seq(), 0);
        assert_eq!(EventLog::restore(10, Some(b"{not json")).latest_seq(), 0);
        // An empty saved ring still continues the sequence.
        let empty = br#"{"next_seq": 42, "events": []}"#;
        assert_eq!(EventLog::restore(10, Some(empty)).latest_seq(), 41);
    }
}
