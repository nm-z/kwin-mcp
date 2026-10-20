//! Lifecycle phases of the session handles one kwin-mcp process holds.
//!
//! A handle's phase comes from two facts the process already keeps: whether a
//! desktop is published, and whether a terminal status is recorded. session_list
//! reports these phases, the desktop tools accept only a running desktop, and
//! session_stop acts on every handle the listing shows. Deriving the phase here,
//! in one place, keeps the three from disagreeing about whether a handle is usable.

use std::time::Duration;

/// A desktop that exited or was killed.
pub(crate) const STATUS_DEAD: &str = "dead";
/// A desktop torn down by session_stop.
pub(crate) const STATUS_STOPPED: &str = "stopped";
/// A desktop that outlived its TTL with no tool activity.
pub(crate) const STATUS_EXPIRED: &str = "expired";
/// A session_start that failed and cleaned up after itself.
pub(crate) const STATUS_START_FAILED: &str = "start_failed";
/// A session_start whose host call stayed blocked past its budget. Its cleanup
/// runs when that call returns.
pub(crate) const STATUS_START_BLOCKED: &str = "start_blocked";

/// Where a handle is in its lifecycle.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    /// session_start has not published a desktop, and no terminal status is set.
    Starting,
    /// A published desktop, which accepts desktop tools.
    Running,
    /// The desktop is gone and a terminal status names why. session_stop clears
    /// the handle.
    Terminal(String),
}

/// The phase, given whether a desktop is published and the recorded terminal
/// status, if any. A terminal status wins: a stop, expiry, or death records its
/// status before it releases the desktop.
pub(crate) fn phase(has_desktop: bool, terminal_status: Option<&str>) -> Phase {
    match terminal_status {
        Some(status) => Phase::Terminal(status.to_owned()),
        None if has_desktop => Phase::Running,
        None => Phase::Starting,
    }
}

/// What session_stop does once it holds the lifecycle gate.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum StopPlan {
    /// Tear down the published desktop, then remove the handle.
    Teardown,
    /// Clean up a terminal handle, then remove it.
    Clear,
    /// A start that never published a desktop and holds no gate was abandoned:
    /// record it as failed, then remove it.
    Abandon,
}

pub(crate) fn stop_plan(phase: &Phase) -> StopPlan {
    match phase {
        Phase::Running => StopPlan::Teardown,
        Phase::Terminal(_) => StopPlan::Clear,
        Phase::Starting => StopPlan::Abandon,
    }
}

/// Whether session_stop answers at once instead of waiting for the lifecycle
/// gate. A blocked start keeps the gate until its host call returns, so its stop
/// is recorded as pending, and the handle stays until that cleanup runs.
pub(crate) fn stop_is_pending(phase: &Phase, gate_busy: bool) -> bool {
    gate_busy && *phase == Phase::Terminal(STATUS_START_BLOCKED.to_owned())
}

/// Whether the reaper expires a handle. Only a running desktop expires, and
/// only once it has been idle for the whole TTL.
pub(crate) fn idle_expired(phase: &Phase, idle: Duration, ttl: Option<Duration>) -> bool {
    *phase == Phase::Running && ttl.is_some_and(|ttl| idle >= ttl)
}

/// How a failed session_start leaves its handle.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FailedStart {
    /// The parameters were rejected before anything was created, so the caller
    /// never received the ID. The handle is forgotten.
    Forget,
    /// The start cleaned up after itself, or kept cleanup ownership. The handle
    /// stays as start_failed until session_stop clears it.
    Failed,
    /// A host call is still blocked. The handle stays as start_blocked until
    /// that call returns and its cleanup runs.
    Blocked,
}

pub(crate) fn failed_start(invalid_params: bool, host_call_blocked: bool) -> FailedStart {
    if host_call_blocked {
        FailedStart::Blocked
    } else if invalid_params {
        FailedStart::Forget
    } else {
        FailedStart::Failed
    }
}

/// Whether clearing a terminal handle writes a lifecycle record. A stop or expiry
/// already recorded its end, so a clear that removes nothing leaves that record
/// alone. A dead desktop never recorded one, so its clear does.
pub(crate) fn clear_writes_record(status: &str, workdir_removed: bool) -> bool {
    workdir_removed || status == STATUS_DEAD
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn terminal(status: &str) -> Phase {
        Phase::Terminal(status.to_owned())
    }

    #[test]
    fn phase_follows_the_desktop_and_the_terminal_status() {
        assert_eq!(phase(false, None), Phase::Starting);
        assert_eq!(phase(true, None), Phase::Running);
        assert_eq!(phase(false, Some(STATUS_DEAD)), terminal(STATUS_DEAD));
        // A stop records its status before it releases the desktop, so a status
        // outranks a desktop that is still set.
        assert_eq!(phase(true, Some(STATUS_STOPPED)), terminal(STATUS_STOPPED));
    }

    #[test]
    fn handle_transitions_keep_one_phase_at_a_time() {
        // session_start creates the handle before its desktop is published.
        assert_eq!(phase(false, None), Phase::Starting);
        // Publishing the desktop makes the handle Running.
        assert_eq!(phase(true, None), Phase::Running);
        // A sandbox exit, an idle expiry, and a stop each leave a terminal status
        // behind, with or without the desktop still being torn down.
        assert_eq!(phase(false, Some(STATUS_DEAD)), terminal(STATUS_DEAD));
        assert_eq!(phase(true, Some(STATUS_EXPIRED)), terminal(STATUS_EXPIRED));
        assert_eq!(phase(false, Some(STATUS_STOPPED)), terminal(STATUS_STOPPED));
        // A failed start never published a desktop and keeps its status.
        assert_eq!(
            phase(false, Some(STATUS_START_FAILED)),
            terminal(STATUS_START_FAILED)
        );
        assert_eq!(
            phase(false, Some(STATUS_START_BLOCKED)),
            terminal(STATUS_START_BLOCKED)
        );
    }

    #[test]
    fn every_phase_has_a_stop_plan() {
        assert_eq!(stop_plan(&Phase::Running), StopPlan::Teardown);
        assert_eq!(stop_plan(&terminal(STATUS_EXPIRED)), StopPlan::Clear);
        assert_eq!(stop_plan(&terminal(STATUS_DEAD)), StopPlan::Clear);
        assert_eq!(stop_plan(&terminal(STATUS_START_BLOCKED)), StopPlan::Clear);
        assert_eq!(stop_plan(&Phase::Starting), StopPlan::Abandon);
    }

    #[test]
    fn stop_of_a_blocked_start_does_not_wait_for_its_gate() {
        let blocked = terminal(STATUS_START_BLOCKED);
        assert!(stop_is_pending(&blocked, true));
        // Once the blocked call returned and the gate is free, the stop clears the
        // handle instead.
        assert!(!stop_is_pending(&blocked, false));
        // Other handles never answer pending: they wait for the gate as usual.
        assert!(!stop_is_pending(&Phase::Running, true));
        assert!(!stop_is_pending(&Phase::Starting, true));
        assert!(!stop_is_pending(&terminal(STATUS_DEAD), true));
    }

    #[test]
    fn idle_running_desktop_expires_once_it_passes_the_ttl() {
        let ttl = Some(Duration::from_secs(120 * 60));
        // The 11-hour idle desktop reported in #514.
        assert!(idle_expired(
            &Phase::Running,
            Duration::from_secs(40_478),
            ttl
        ));
        assert!(idle_expired(
            &Phase::Running,
            Duration::from_secs(7_200),
            ttl
        ));
        assert!(!idle_expired(
            &Phase::Running,
            Duration::from_secs(7_199),
            ttl
        ));
        // Without a TTL, nothing expires.
        assert!(!idle_expired(
            &Phase::Running,
            Duration::from_secs(40_478),
            None
        ));
        // Only a running desktop expires. A starting or terminal handle is never
        // expired again.
        assert!(!idle_expired(
            &Phase::Starting,
            Duration::from_secs(40_478),
            ttl
        ));
        assert!(!idle_expired(
            &terminal(STATUS_STOPPED),
            Duration::from_secs(40_478),
            ttl
        ));
    }

    #[test]
    fn failed_start_keeps_or_forgets_its_handle_by_what_it_left_behind() {
        // A blocked host call keeps the handle, whatever else is true.
        assert_eq!(failed_start(false, true), FailedStart::Blocked);
        // Rejected parameters never created anything, so there is nothing to keep.
        assert_eq!(failed_start(true, false), FailedStart::Forget);
        // Any other failure ran its cleanup and leaves a start_failed handle.
        assert_eq!(failed_start(false, false), FailedStart::Failed);
    }

    #[test]
    fn clearing_a_handle_writes_a_record_only_when_one_is_missing_or_changed() {
        // A stop or expiry that removed nothing keeps the record it already wrote.
        assert!(!clear_writes_record(STATUS_STOPPED, false));
        assert!(!clear_writes_record(STATUS_EXPIRED, false));
        // A clear that removed the workdir records that it did.
        assert!(clear_writes_record(STATUS_EXPIRED, true));
        assert!(clear_writes_record(STATUS_START_FAILED, true));
        // A dead desktop never recorded its end, so clearing it does.
        assert!(clear_writes_record(STATUS_DEAD, false));
    }
}
