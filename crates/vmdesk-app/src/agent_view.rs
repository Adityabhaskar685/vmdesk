//! What the VM header says about the VM's agent, and the one action it offers there.

use chrono::{DateTime, Utc};
use vmdesk_core::AgentState;
use vmdesk_db::AgentRecord;

use crate::human::ago;
use crate::theme::Tone;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentAction {
    Install,
    Upgrade,
    /// Asks for confirmation first.
    Remove,
}

impl AgentAction {
    pub fn label(self) -> &'static str {
        match self {
            Self::Install => "Install agent",
            Self::Upgrade => "Upgrade agent",
            Self::Remove => "Remove agent",
        }
    }

    /// Element id of the action's button.
    pub fn id(self) -> &'static str {
        match self {
            Self::Install => "agent-install",
            Self::Upgrade => "agent-upgrade",
            Self::Remove => "agent-remove",
        }
    }

    /// Shown while the action runs.
    pub fn busy(self) -> &'static str {
        match self {
            Self::Install => "installing agent",
            Self::Upgrade => "upgrading agent",
            Self::Remove => "removing agent",
        }
    }
}

/// The agent as the header shows it, from the VM's agent record (written by the first
/// sync that got an answer) and the agent state in its status (written by every sync).
#[derive(Debug, PartialEq, Eq)]
pub enum AgentView {
    /// Never installed, or removed.
    None,
    /// A sync found the binary gone from the VM.
    Missing,
    Ok {
        version: String,
        synced: Option<DateTime<Utc>>,
    },
    Outdated {
        version: String,
        synced: Option<DateTime<Utc>>,
    },
    /// Installed, but its daemon is stopped: the next poll starts it.
    NotRunning,
    /// It did not answer properly. The reason is only in the log of the sync that failed.
    Error,
    /// Installed; no sync has reported on it since the VM was last reached.
    Unchecked {
        version: String,
        synced: Option<DateTime<Utc>>,
    },
}

impl AgentView {
    pub fn new(record: Option<&AgentRecord>, state: AgentState) -> Self {
        let Some(a) = record else {
            return Self::None;
        };
        let (version, synced) = (a.version.clone(), a.last_contact_at);
        match state {
            AgentState::Absent => Self::Missing,
            AgentState::Ok => Self::Ok { version, synced },
            AgentState::Outdated => Self::Outdated { version, synced },
            AgentState::NotRunning => Self::NotRunning,
            AgentState::Error => Self::Error,
            AgentState::Unknown => Self::Unchecked { version, synced },
        }
    }

    pub fn text(&self, now: DateTime<Utc>) -> String {
        let synced = |at: &Option<DateTime<Utc>>| match at {
            Some(t) => format!("synced {} ago", ago((now - *t).num_seconds())),
            None => "never synced".to_owned(),
        };
        match self {
            Self::None => "No agent".to_owned(),
            Self::Missing => "Agent missing from the VM · collecting over SSH".to_owned(),
            Self::Ok {
                version,
                synced: at,
            } => format!("Agent {version} · recording · {}", synced(at)),
            Self::Outdated {
                version,
                synced: at,
            } => format!("Agent {version} · outdated · {}", synced(at)),
            Self::NotRunning => "Agent not running · the next poll starts it".to_owned(),
            Self::Error => "Agent not answering properly · collecting over SSH".to_owned(),
            Self::Unchecked {
                version,
                synced: at,
            } => format!("Agent {version} · not checked yet · {}", synced(at)),
        }
    }

    pub fn tone(&self) -> Tone {
        match self {
            Self::Ok { .. } => Tone::Good,
            Self::Missing | Self::Outdated { .. } | Self::NotRunning => Tone::Warn,
            Self::Error => Tone::Bad,
            Self::None | Self::Unchecked { .. } => Tone::Neutral,
        }
    }

    pub fn action(&self) -> AgentAction {
        match self {
            Self::None | Self::Missing => AgentAction::Install,
            Self::Outdated { .. } => AgentAction::Upgrade,
            Self::Ok { .. } | Self::NotRunning | Self::Error | Self::Unchecked { .. } => {
                AgentAction::Remove
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::Duration;
    use uuid::Uuid;
    use vmdesk_core::VmId;

    use super::*;

    #[test]
    fn each_state_reads_and_offers_one_action() {
        let now = Utc::now();
        let record = AgentRecord {
            vm: VmId::new(),
            agent_id: Uuid::nil(),
            version: "0.1.0".into(),
            proto: 1,
            installed_at: None,
            last_seq: 0,
            last_contact_at: Some(now - Duration::seconds(130)),
        };
        let cases = [
            (None, AgentState::Unknown, "No agent", AgentAction::Install),
            (None, AgentState::Absent, "No agent", AgentAction::Install),
            (
                Some(&record),
                AgentState::Absent,
                "Agent missing from the VM · collecting over SSH",
                AgentAction::Install,
            ),
            (
                Some(&record),
                AgentState::Ok,
                "Agent 0.1.0 · recording · synced 2m ago",
                AgentAction::Remove,
            ),
            (
                Some(&record),
                AgentState::Outdated,
                "Agent 0.1.0 · outdated · synced 2m ago",
                AgentAction::Upgrade,
            ),
            (
                Some(&record),
                AgentState::NotRunning,
                "Agent not running · the next poll starts it",
                AgentAction::Remove,
            ),
            (
                Some(&record),
                AgentState::Error,
                "Agent not answering properly · collecting over SSH",
                AgentAction::Remove,
            ),
            (
                Some(&record),
                AgentState::Unknown,
                "Agent 0.1.0 · not checked yet · synced 2m ago",
                AgentAction::Remove,
            ),
        ];
        for (record, state, text, action) in cases {
            let view = AgentView::new(record, state);
            assert_eq!(
                (view.text(now).as_str(), view.action()),
                (text, action),
                "{state}"
            );
        }
    }
}
