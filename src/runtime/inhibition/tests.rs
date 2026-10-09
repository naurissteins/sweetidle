use super::{InhibitionState, Transition};
use crate::session::{DbusInhibitEvent, InhibitorInfo, SystemdIdleInhibitor};

#[test]
fn starts_without_inhibitors_or_details() {
    let state = InhibitionState::default();
    assert!(!state.inhibited());
    assert_eq!(state.dbus_count(), 0);
    assert_eq!(state.systemd_count(), 0);
    assert_eq!(state.dbus_details().count(), 0);
    assert!(state.systemd_details().is_empty());
}

#[test]
fn either_source_changes_presence_only_at_the_first_addition_or_final_release() {
    let cases = [
        (false, false, false, Transition::Unchanged),
        (false, false, true, Transition::Started),
        (true, false, false, Transition::Ended),
        (true, false, true, Transition::Unchanged),
        (false, true, false, Transition::Unchanged),
        (false, true, true, Transition::Unchanged),
        (true, true, false, Transition::Unchanged),
        (true, true, true, Transition::Unchanged),
    ];
    for dbus_changes in [true, false] {
        for (previous, other, next, expected) in cases {
            let mut state = InhibitionState::default();
            let (dbus, systemd) = if dbus_changes {
                (previous, other)
            } else {
                (other, previous)
            };
            state.apply_dbus(&dbus_event(dbus));
            state.apply_systemd(systemd_snapshot(systemd));

            let transition = if dbus_changes {
                state.apply_dbus(&dbus_event(next))
            } else {
                state.apply_systemd(systemd_snapshot(next))
            };
            assert_eq!(
                transition, expected,
                "dbus_changes={dbus_changes}, previous={previous}, other={other}, next={next}"
            );
            assert_eq!(state.inhibited(), next || other);
        }
    }
}

#[test]
fn owner_disconnect_removes_only_that_owners_details() {
    let mut state = InhibitionState::default();
    for cookie in 1..=3 {
        let owner = if cookie < 3 { ":1.1" } else { ":1.2" };
        state.apply_dbus(&DbusInhibitEvent::Added {
            inhibitor: inhibitor(cookie, owner),
            active_count: cookie as usize,
        });
    }
    assert_eq!(
        state.apply_dbus(&DbusInhibitEvent::OwnerDisconnected {
            owner: ":1.1".to_string(),
            released_count: 2,
            active_count: 1,
        }),
        Transition::Unchanged
    );
    assert_eq!(state.dbus_count(), 1);
    assert_eq!(
        state.dbus_details().collect::<Vec<_>>(),
        vec![&inhibitor(3, ":1.2")]
    );

    assert_eq!(
        state.apply_dbus(&DbusInhibitEvent::Removed {
            inhibitor: inhibitor(3, ":1.2"),
            active_count: 0,
        }),
        Transition::Ended
    );
    assert_eq!(state.dbus_details().count(), 0);
}

#[test]
fn systemd_snapshots_replace_details_without_restarting_inhibition() {
    let mut state = InhibitionState::default();
    assert_eq!(
        state.apply_systemd(systemd_snapshot(true)),
        Transition::Started
    );
    let same_count_replacement = vec![systemd_inhibitor(20)];
    assert_eq!(
        state.apply_systemd(same_count_replacement.clone()),
        Transition::Unchanged
    );
    assert_eq!(state.systemd_count(), 1);
    assert_eq!(state.systemd_details(), same_count_replacement);
    let replacement = vec![systemd_inhibitor(20), systemd_inhibitor(30)];
    assert_eq!(
        state.apply_systemd(replacement.clone()),
        Transition::Unchanged
    );
    assert_eq!(state.systemd_count(), 2);
    assert_eq!(state.systemd_details(), replacement);
    assert_eq!(state.apply_systemd(Vec::new()), Transition::Ended);
    assert_eq!(state.systemd_count(), 0);
    assert!(state.systemd_details().is_empty());
}

#[test]
fn dbus_worker_count_remains_authoritative_when_details_are_incomplete() {
    let mut state = InhibitionState::default();
    assert_eq!(
        state.apply_dbus(&DbusInhibitEvent::Added {
            inhibitor: inhibitor(1, ":1.1"),
            active_count: 2,
        }),
        Transition::Started
    );
    assert_eq!(
        state.apply_dbus(&DbusInhibitEvent::Removed {
            inhibitor: inhibitor(99, ":1.99"),
            active_count: 1,
        }),
        Transition::Unchanged
    );
    assert_eq!(
        state.apply_dbus(&DbusInhibitEvent::Removed {
            inhibitor: inhibitor(1, ":1.1"),
            active_count: 1,
        }),
        Transition::Unchanged
    );
    assert_eq!(state.dbus_count(), 1);
    assert!(state.inhibited());
    assert_eq!(state.dbus_details().count(), 0);
    assert_eq!(
        state.apply_dbus(&DbusInhibitEvent::OwnerDisconnected {
            owner: ":1.99".to_string(),
            released_count: 1,
            active_count: 0,
        }),
        Transition::Ended
    );
}

#[test]
fn repeated_cookie_updates_replace_diagnostic_details() {
    let mut state = InhibitionState::default();
    state.apply_dbus(&dbus_event(true));
    let mut updated = inhibitor(1, ":1.1");
    updated.application = "updated application".to_string();
    assert_eq!(
        state.apply_dbus(&DbusInhibitEvent::Added {
            inhibitor: updated.clone(),
            active_count: 1,
        }),
        Transition::Unchanged
    );
    assert_eq!(state.dbus_details().collect::<Vec<_>>(), vec![&updated]);
}

fn dbus_event(active: bool) -> DbusInhibitEvent {
    if active {
        DbusInhibitEvent::Added {
            inhibitor: inhibitor(1, ":1.1"),
            active_count: 1,
        }
    } else {
        DbusInhibitEvent::Removed {
            inhibitor: inhibitor(1, ":1.1"),
            active_count: 0,
        }
    }
}

fn inhibitor(cookie: u32, owner: &str) -> InhibitorInfo {
    InhibitorInfo {
        cookie,
        owner: owner.to_string(),
        application: "test application".to_string(),
        reason: "test reason".to_string(),
    }
}

fn systemd_snapshot(active: bool) -> Vec<SystemdIdleInhibitor> {
    if active {
        vec![systemd_inhibitor(10)]
    } else {
        Vec::new()
    }
}

fn systemd_inhibitor(pid: u32) -> SystemdIdleInhibitor {
    SystemdIdleInhibitor {
        who: "test application".to_string(),
        why: "test reason".to_string(),
        uid: 1000,
        pid,
    }
}
