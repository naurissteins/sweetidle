use std::io::{self, Read};
use std::time::Duration;

use calloop::{EventLoop, Interest, Mode, PostAction, generic::Generic};
use rustix::process::{Pid, PidfdFlags, pidfd_open};

use crate::{
    command::CommandContext,
    config::Config,
    listener::{CommandIntent, CommandKind, ListenerId, ListenerSet},
    runtime::{
        RuntimeError, RuntimeEvent,
        state::RuntimeState,
        test_support::{
            assert_inhibitor_released, new_event_loop, observable_inhibitor,
            wait_for_process_events,
        },
    },
    wayland::IdleWaylandState,
};

#[test]
fn launches_and_reaps_commands_through_pidfds() -> Result<(), RuntimeError> {
    let mut event_loop = new_event_loop();
    let mut state = runtime_state();
    let mut events = Vec::new();
    state
        .commands
        .dispatch_listener(&event_loop.handle(), intent("exit 0"), &mut events)?;
    assert!(matches!(
        events.as_slice(),
        [RuntimeEvent::CommandStarted(_)]
    ));

    let events = wait_for_process_events(&mut event_loop, &mut state);
    assert!(matches!(
        events.as_slice(),
        [RuntimeEvent::CommandFinished { completion, .. }] if completion.status.success()
    ));
    assert!(state.commands.is_empty());
    Ok(())
}

#[test]
fn skips_duplicate_commands_while_the_first_process_runs() -> Result<(), RuntimeError> {
    let mut event_loop = new_event_loop();
    let mut state = runtime_state();
    let mut events = Vec::new();
    for _ in 0..2 {
        state.commands.dispatch_listener(
            &event_loop.handle(),
            intent("sleep 0.05"),
            &mut events,
        )?;
    }
    assert!(matches!(
        events.as_slice(),
        [
            RuntimeEvent::CommandStarted(_),
            RuntimeEvent::CommandSkipped(_)
        ]
    ));

    let _ = wait_for_process_events(&mut event_loop, &mut state);
    assert!(state.commands.is_empty());
    Ok(())
}

#[test]
fn holds_the_sleep_inhibitor_until_the_command_finishes() -> Result<(), RuntimeError> {
    let mut event_loop = new_event_loop();
    let mut state = runtime_state();
    let mut events = Vec::new();
    let (mut observer, inhibitor) = observable_inhibitor();
    state.commands.dispatch_session(
        &event_loop.handle(),
        CommandContext::BeforeSleep,
        "exit 0".to_string(),
        Some(inhibitor),
        &mut events,
    )?;
    assert!(
        matches!(observer.read(&mut [0]), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
    );

    let _ = wait_for_process_events(&mut event_loop, &mut state);
    assert_inhibitor_released(&mut observer);
    assert!(state.commands.is_empty());
    Ok(())
}

#[test]
fn failed_sleep_hook_releases_its_slot_and_inhibitor_without_command_text()
-> Result<(), RuntimeError> {
    let event_loop = new_event_loop();
    let mut state = runtime_state();
    let mut events = Vec::new();
    let (mut observer, inhibitor) = observable_inhibitor();
    state.commands.dispatch_session(
        &event_loop.handle(),
        CommandContext::BeforeSleep,
        "printf secret-command-token\0".to_string(),
        Some(inhibitor),
        &mut events,
    )?;
    assert!(matches!(
        events.as_slice(),
        [RuntimeEvent::CommandFailed { command, error }]
            if command.context == CommandContext::BeforeSleep
                && matches!(error, crate::ProcessError::Spawn { command_id, .. }
                    if *command_id == command.command_id)
    ));
    assert!(!format!("{events:?}").contains("secret-command-token"));
    assert_inhibitor_released(&mut observer);
    assert!(state.commands.is_empty());
    Ok(())
}

#[test]
fn late_pidfd_callback_does_not_repeat_an_already_collected_completion()
-> Result<(), Box<dyn std::error::Error>> {
    let event_loop = new_event_loop();
    let mut state = runtime_state();
    let mut events = Vec::new();
    for index in 0..2 {
        let mut intent = intent("exit 0");
        intent.listener_id = ListenerId::from_index(index);
        state
            .commands
            .dispatch_listener(&event_loop.handle(), intent, &mut events)?;
    }
    let starts = events
        .iter()
        .filter_map(|event| match event {
            RuntimeEvent::CommandStarted(start) => Some(*start),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(starts.len(), 2);
    for start in &starts {
        wait_for_exit(start.process_id)?;
    }

    // both children have exited, but only the first callback has reached the runtime
    events.clear();
    state.commands.mark_process_ready(starts[0].command_id);
    state.commands.collect_ready_processes(&mut events)?;
    assert_eq!(events.len(), 2);
    for start in &starts {
        assert!(events.iter().any(|event| matches!(event,
            RuntimeEvent::CommandFinished { command, completion }
                if command.command_id == start.command_id && completion.status.success()
        )));
    }
    events.clear();
    state.commands.mark_process_ready(starts[1].command_id);
    state.commands.collect_ready_processes(&mut events)?;
    assert!(events.is_empty());
    assert!(state.commands.is_empty());
    Ok(())
}

fn wait_for_exit(process_id: u32) -> Result<(), Box<dyn std::error::Error>> {
    let pid = i32::try_from(process_id)
        .ok()
        .and_then(Pid::from_raw)
        .ok_or("invalid test PID")?;
    let pidfd = pidfd_open(pid, PidfdFlags::NONBLOCK)?;
    let mut event_loop = EventLoop::<bool>::try_new()?;
    event_loop
        .handle()
        .insert_source(
            Generic::new(pidfd, Interest::READ, Mode::Level),
            |_, _, exited| {
                *exited = true;
                Ok::<PostAction, io::Error>(PostAction::Remove)
            },
        )
        .map_err(|error| error.error)?;
    let mut exited = false;
    event_loop.dispatch(Some(Duration::from_secs(2)), &mut exited)?;
    assert!(exited, "command did not exit");
    Ok(())
}

fn runtime_state() -> RuntimeState {
    let config = Config::default();
    RuntimeState::new(
        IdleWaylandState::default(),
        ListenerSet::from_config(&config),
        &config,
        None,
    )
}

fn intent(command: &str) -> CommandIntent {
    CommandIntent {
        listener_id: ListenerId::from_index(0),
        kind: CommandKind::Timeout,
        command: command.to_string(),
    }
}
