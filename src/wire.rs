//! Glue between the buddy pieces: quick ask and agents drive the island,
//! Apeiron gestures pause Alt-hold. Each piece stays unaware of the others.

use crate::agents::{self, Agent, Status};
use crate::island::{self, AgentRow, AgentStatus, BuddyState};
use crate::{apeiron, keys, quick};
use std::collections::HashSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Unix seconds when the user last looked at agents (opened the palette or an
/// agent). Agents that finished after this count as unseen on the island.
static SEEN: AtomicU64 = AtomicU64::new(0);
/// Finished agents whose card the user opened since.
static SEEN_IDS: Mutex<Option<HashSet<String>>> = Mutex::new(None);

/// Hooks everything up (once, after the island, quick ask and Apeiron start).
pub fn start() {
    quick::set_status_sink(|s| {
        island::buddy(match s {
            quick::Status::Idle => BuddyState::Idle,
            quick::Status::Listening(l) => BuddyState::Listening(l),
            quick::Status::Thinking => BuddyState::Thinking,
            quick::Status::Streaming => BuddyState::Streaming,
            quick::Status::Done => BuddyState::Done,
            quick::Status::Error(_) => BuddyState::Error,
        })
    });

    // Apeiron wakes us on layout changes (gesture events included); only
    // tell the hook when the answer flips.
    static GESTURE: AtomicBool = AtomicBool::new(false);
    apeiron::add_waker(|| {
        let on = apeiron::gesture_active();
        if GESTURE.swap(on, Ordering::Relaxed) != on {
            keys::set_suppressed(on);
        }
    });

    SEEN.store(now(), Ordering::Relaxed);
    let rx = agents::subscribe();
    let _ = std::thread::Builder::new()
        .name("screenbuddy-agent-rows".into())
        .spawn(move || {
            push_rows();
            while rx.recv().is_ok() {
                // Steps can come in bursts; one refresh per burst is enough.
                while rx.recv_timeout(Duration::from_millis(50)).is_ok() {}
                push_rows();
            }
        });
}

/// The user opened this agent's card: its island row is no longer new.
pub fn mark_agent_seen(id: &str) {
    let fresh = SEEN_IDS
        .lock()
        .unwrap()
        .get_or_insert_with(HashSet::new)
        .insert(id.to_string());
    if fresh {
        push_rows();
    }
}

/// The user looked at agents: clear the island's "new" marks.
pub fn mark_seen() {
    SEEN.store(now(), Ordering::Relaxed);
    *SEEN_IDS.lock().unwrap() = None;
    push_rows();
}

fn push_rows() {
    if let Some(h) = island::handle() {
        let seen = SEEN.load(Ordering::Relaxed);
        let opened = SEEN_IDS.lock().unwrap().clone().unwrap_or_default();
        h.set_agents(
            agents::list()
                .iter()
                .map(|a| {
                    let mut r = row(a, seen);
                    // Opening a card covers its run; a new run is new again.
                    if r.kind != AgentStatus::Asking && opened.contains(&a.id) {
                        r.unseen = false;
                    }
                    r
                })
                .collect(),
        );
    }
}

fn row(a: &Agent, seen: u64) -> AgentRow {
    let kind = match (a.status, &a.ask) {
        (_, Some(_)) if a.status.is_active() => AgentStatus::Asking,
        (Status::Pending, _) => AgentStatus::Pending,
        (Status::Running, _) => AgentStatus::Running,
        (Status::Done, _) => AgentStatus::Done,
        (Status::Failed, _) => AgentStatus::Failed,
        (Status::Stopped, _) => AgentStatus::Stopped,
    };
    let first_line = |s: &str| {
        s.lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let status = match kind {
        AgentStatus::Asking => match &a.ask {
            Some(q) if q.kind == agents::AskKind::Approval => "Needs your OK".to_string(),
            Some(q) => first_line(&q.text),
            None => String::new(),
        },
        AgentStatus::Pending => "Starting".into(),
        AgentStatus::Running => a
            .steps
            .last()
            .map(|s| first_line(s))
            .unwrap_or_else(|| "Working".into()),
        AgentStatus::Done => Some(first_line(&a.result))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "Done".into()),
        AgentStatus::Failed => a
            .error
            .as_deref()
            .map(first_line)
            .unwrap_or_else(|| "Failed".into()),
        AgentStatus::Stopped => "Stopped".into(),
    };
    let finished_unseen = !a.status.is_active() && a.ended.is_some_and(|e| e >= seen);
    AgentRow {
        id: a.id.clone(),
        name: a.name.clone(),
        status,
        kind,
        started: UNIX_EPOCH + Duration::from_secs(a.started.unwrap_or(a.created)),
        unseen: kind == AgentStatus::Asking || finished_unseen,
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_map_status_and_unseen() {
        let mut a = Agent {
            id: "x".into(),
            name: "Flights".into(),
            status: Status::Running,
            created: 100,
            steps: vec!["Searching\nmore".into()],
            ..Default::default()
        };
        let r = row(&a, 50);
        assert_eq!(
            (r.kind, r.status.as_str(), r.unseen),
            (AgentStatus::Running, "Searching", false)
        );
        a.status = Status::Done;
        a.ended = Some(200);
        a.result = "Cheapest is KLM".into();
        assert!(row(&a, 150).unseen);
        assert!(!row(&a, 250).unseen);
        assert_eq!(row(&a, 250).status, "Cheapest is KLM");
    }
}
