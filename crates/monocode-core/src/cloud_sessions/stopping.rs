//! What became of a Stop, told from read-only listings and never from the stop call alone.
//!
//! `sbx --cloud stop` only says the request was accepted ("Stop returns once the request is
//! accepted"). A sandbox with Auto-resume can start again afterwards, so "the call succeeded"
//! is not "it is stopped". This module remembers each stop this process asked for, and every
//! workspaces overview load (`sbx ls`, which never wakes anything) is read against that
//! memory:
//!
//! * the listing says stopping: [`StopReport::Stopping`], and [`StopReport::TakingLong`] once it
//!   has been stopping for [`STOP_EXPECTED`] (the CLI says a large sandbox can take 7 to 9
//!   minutes);
//! * the listing says stopped: [`StopReport::Stopped`];
//! * the listing still says running after [`STOP_GRACE`], or says running again later:
//!   [`StopReport::StillRunning`] with the reason. It stays that way until the user stops it
//!   again or the workspace is seen stopped, so a workspace is never quietly "Running" again.
//!
//! Since when a workspace has been stopping ([`super::workspaces::WorkspaceRow::stopping_since_ms`])
//! is kept here too: from the request when this app made it, else from the first listing that
//! showed it. The footer chip warns when that passes [`STOPPING_WARN`].
use super::workspaces::{WorkspaceRow, WorkspaceState};
use super::{CloudRef, CloudSessions};
use std::collections::HashMap;
use std::time::Duration;

/// A listing that still says running this soon after the request has probably not caught up.
pub const STOP_GRACE: Duration = Duration::from_secs(60);
/// A stop that takes longer than this is reported as taking longer than expected.
pub const STOP_EXPECTED: Duration = Duration::from_secs(10 * 60);
/// The footer chip turns to a warning after a workspace has been stopping this long.
pub const STOPPING_WARN: Duration = Duration::from_secs(15 * 60);

/// What the dialog says about a workspace after a Stop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopReport {
    /// The stop was accepted and the workspace is on its way down.
    Stopping,
    /// Still on its way down after [`STOP_EXPECTED`].
    TakingLong,
    Stopped,
    /// It is running although a stop was accepted; the text says why that can be.
    StillRunning(String),
}

impl StopReport {
    /// Whether the answer can still change by itself (the dialog keeps reading).
    pub fn unsettled(&self) -> bool {
        matches!(self, Self::Stopping | Self::TakingLong)
    }
}

/// How this app's own connection to the workspace fared before the stop was sent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) enum Connection {
    /// Nothing of this app was connected to it.
    #[default]
    NoneOpen,
    /// This many connections were closed first.
    Closed(usize),
    /// Closing failed; the stop was sent anyway.
    CloseFailed(String),
}

#[derive(Clone, Debug, Default)]
pub(super) struct StopWatch {
    /// When this process had the stop accepted; cleared once the listing settled.
    requested_ms: Option<u64>,
    /// Since when the listing has said stopping.
    stopping_since_ms: Option<u64>,
    /// A verdict that stays until the state changes again.
    settled: Option<StopReport>,
    connection: Connection,
}

fn reason(connection: &Connection) -> String {
    let ours = match connection {
        Connection::NoneOpen => "Monoptah had no connection to it open.".to_owned(),
        Connection::Closed(_) => {
            "Monoptah closed its own connection to it before stopping.".to_owned()
        }
        Connection::CloseFailed(error) => {
            format!("Monoptah could not close its own connection to it before stopping ({error}).")
        }
    };
    format!(
        "it was running again after the stop was accepted. A sandbox with Auto-resume starts again when anything sends it traffic: a command, or a request through an ssh tunnel that is still open (a tunnel survives a stop and the sandbox's gateway then resumes it). {ours}"
    )
}

/// The report of one row and the since-when of its stopping, from what the listing says now.
fn read(
    watch: &mut StopWatch,
    state: WorkspaceState,
    now: u64,
) -> (Option<StopReport>, Option<u64>) {
    match state {
        WorkspaceState::Stopping | WorkspaceState::Hibernating => {
            watch.settled = None;
            let since = *watch
                .stopping_since_ms
                .get_or_insert(watch.requested_ms.unwrap_or(now));
            let slow = now.saturating_sub(since) >= STOP_EXPECTED.as_millis() as u64;
            // Only a Stop this app asked for is reported on (and followed by the dialog): one
            // somebody else started, or the TTL, shows in the row's state and in the chip.
            let report = watch.requested_ms.map(|_| {
                if slow {
                    StopReport::TakingLong
                } else {
                    StopReport::Stopping
                }
            });
            (report, Some(since))
        }
        WorkspaceState::Stopped => {
            watch.stopping_since_ms = None;
            if watch.requested_ms.take().is_some() {
                watch.settled = Some(StopReport::Stopped);
            }
            (watch.settled.clone(), None)
        }
        WorkspaceState::Running | WorkspaceState::Starting => {
            watch.stopping_since_ms = None;
            if let Some(requested) = watch.requested_ms {
                if now.saturating_sub(requested) < STOP_GRACE.as_millis() as u64 {
                    return (Some(StopReport::Stopping), None);
                }
                watch.requested_ms = None;
                watch.settled = Some(StopReport::StillRunning(reason(&watch.connection)));
            } else if watch.settled == Some(StopReport::Stopped) {
                // Started again on purpose or by someone else: the old verdict is history.
                watch.settled = None;
            }
            (watch.settled.clone(), None)
        }
        WorkspaceState::Failed | WorkspaceState::Unknown => (watch.settled.clone(), None),
    }
}

#[derive(Default)]
pub(super) struct StopWatches(HashMap<String, StopWatch>);

#[cfg(test)]
impl StopWatches {
    /// How many workspaces are being followed.
    pub(super) fn followed(&self) -> usize {
        self.0.len()
    }
}

impl CloudSessions {
    /// Remembers that the stop of `cloud` was accepted at the current time. `connection` is how
    /// this app's own connection fared before it.
    pub(super) fn note_stop_accepted(&self, cloud: &CloudRef, connection: Connection) {
        let now = (self.clock)();
        if let Ok(mut watches) = self.stops.lock() {
            watches.0.insert(
                cloud.key(),
                StopWatch {
                    requested_ms: Some(now),
                    stopping_since_ms: None,
                    settled: None,
                    connection,
                },
            );
        }
    }

    /// Forgets what was noted about `cloud` (it is gone, or the user resumed it).
    pub(super) fn forget_stop(&self, cloud: &CloudRef) {
        if let Ok(mut watches) = self.stops.lock() {
            watches.0.remove(&cloud.key());
        }
    }

    /// Closes the connections this app still holds to a sandbox the listing says is stopping or
    /// stopped. A tunnel survives a stop (its keepalives are answered by the gateway), and the
    /// first request a session sends through it resumes the sandbox again: that is what a Stop
    /// from this app, another computer or the TTL would otherwise lose to. A sandbox that a
    /// user-initiated reconnect is bringing back is left alone.
    pub(super) fn close_connections_of_stopped(&self, rows: &[WorkspaceRow]) {
        for row in rows {
            let down = matches!(
                row.state,
                WorkspaceState::Stopping | WorkspaceState::Hibernating | WorkspaceState::Stopped
            );
            if !down || row.cloud.kind != super::CloudBackendKind::DockerSandbox {
                continue;
            }
            let bringing_back = self
                .reconnecting
                .lock()
                .map_or(true, |set| set.contains(&row.cloud.name));
            if !bringing_back {
                let _ = self.close_connections_before_stop(&row.cloud);
            }
        }
    }

    /// Reads every row of a freshly listed overview against the stops noted so far.
    pub(super) fn apply_stop_reports(&self, rows: &mut [WorkspaceRow], now: u64) {
        let Ok(mut watches) = self.stops.lock() else {
            return;
        };
        for row in rows {
            let key = row.cloud.key();
            let tracked = watches.0.contains_key(&key);
            let stopping = matches!(
                row.state,
                WorkspaceState::Stopping | WorkspaceState::Hibernating
            );
            if !tracked && !stopping {
                continue;
            }
            let watch = watches.0.entry(key.clone()).or_default();
            let (report, since) = read(watch, row.state, now);
            row.stop = report;
            row.stopping_since_ms = since;
            if watch.requested_ms.is_none()
                && watch.stopping_since_ms.is_none()
                && watch.settled.is_none()
            {
                watches.0.remove(&key);
            }
        }
    }
}
