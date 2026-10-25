//! A host's way in while a turn is still running.
//!
//! Without this the agent spends a whole turn and only then reads what the
//! user typed meanwhile. With it, the run asks the queue between requests and
//! lands what arrived as a `<user-interrupt>` message, so the model
//! course-corrects inside the same turn instead of after it.
//!
//! The queue only carries messages. A host with commands of its own
//! (`/compact`) keeps them and hands them over as the next run instead,
//! because a compaction rewrites the history later messages land in.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::{AgentInput, ExtractedCommand, InterruptSource};

/// A session's inbox for the window, shared by the thread that drives the
/// agent and the one that reads the window.
#[derive(Clone)]
pub struct InterruptQueue {
    inner: Arc<Inner>,
}

struct Inner {
    queue: Mutex<VecDeque<AgentInput>>,
    /// The session loop's blocking half. Kept here, because every handle
    /// counts as a sender: while one lives a wake always finds a receiver, and
    /// when the last one goes the loop's `recv` fails and the loop ends.
    wake: flume::Sender<()>,
}

impl Default for InterruptQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl InterruptQueue {
    pub fn new() -> Self {
        Self::with_wake(flume::bounded(1))
    }

    /// With the receiving end of the wake channel held by the caller: a
    /// session loop blocks on it between runs.
    pub fn with_wake(wake: (flume::Sender<()>, flume::Receiver<()>)) -> Self {
        Self {
            inner: Arc::new(Inner {
                queue: Mutex::new(VecDeque::new()),
                wake: wake.0,
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, VecDeque<AgentInput>> {
        self.inner
            .queue
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// From the window: the run picks it up before its next request.
    pub fn push(&self, input: AgentInput) {
        self.lock().push_back(input);
    }

    pub fn is_empty(&self) -> bool {
        self.lock().is_empty()
    }

    /// Called when the user sends while the session is doing nothing: the
    /// session loop then starts a run for what is queued. Only the first of a
    /// burst leaves a signal.
    pub fn wake(&self) {
        let _ = self.inner.wake.try_send(());
    }

    /// Takes one message, so a loop can tell whether anything is waiting.
    pub fn take(&self) -> Option<AgentInput> {
        self.lock().pop_front()
    }

    /// Drains every message waiting: they all ride on one run.
    pub fn take_all(&self) -> Vec<AgentInput> {
        self.lock().drain(..).collect()
    }
}

impl InterruptSource for InterruptQueue {
    fn poll(&self) -> Option<ExtractedCommand> {
        let inputs = self.take_all();
        (!inputs.is_empty()).then_some(ExtractedCommand::Interrupt(inputs))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AgentInput, AgentMode, InputSource};
    use maki_config::SessionDefaults;

    fn input(text: &str) -> AgentInput {
        AgentInput::from_defaults(
            text.to_owned(),
            AgentMode::Build,
            Vec::new(),
            SessionDefaults::default(),
            InputSource::Gui,
        )
    }

    #[test]
    fn a_run_takes_everything_waiting_as_one_interrupt() {
        let queue = InterruptQueue::new();
        queue.push(input("first"));
        queue.push(input("second"));

        match queue.poll() {
            Some(ExtractedCommand::Interrupt(inputs)) => {
                let texts: Vec<_> = inputs.iter().map(|i| i.message.as_str()).collect();
                assert_eq!(texts, ["first", "second"]);
            }
            _ => panic!("queued messages must arrive as one interrupt"),
        }
        assert!(queue.poll().is_none(), "nothing is offered twice");
    }

    #[test]
    fn one_wake_covers_a_burst() {
        let (tx, rx) = flume::bounded(1);
        let queue = InterruptQueue::with_wake((tx, rx.clone()));
        queue.wake();
        queue.wake();
        assert!(rx.recv_timeout(std::time::Duration::from_secs(1)).is_ok());
        assert!(rx.try_recv().is_err(), "a wake is a signal, not a count");
    }

    #[test]
    fn the_last_dropped_queue_releases_a_waiting_loop() {
        let (tx, rx) = flume::bounded(1);
        let queue = InterruptQueue::with_wake((tx, rx.clone()));
        drop(queue);
        assert!(
            rx.try_recv().is_err(),
            "the loop wakes on the closed channel"
        );
        assert!(rx.recv().is_err(), "and no sender is left");
    }
}

#[cfg(test)]
mod steering {
    use std::sync::Arc;

    use super::*;
    use crate::History;
    use crate::agent::sync_tests::*;
    use crate::agent::{ScriptedProvider, end_turn};
    use crate::{AgentInput, AgentMode, InputSource};
    use maki_config::SessionDefaults;

    fn typed(text: &str) -> AgentInput {
        AgentInput::from_defaults(
            text.to_owned(),
            AgentMode::Build,
            Vec::new(),
            SessionDefaults::default(),
            InputSource::Gui,
        )
    }

    /// The point of the queue: what the user types while a turn runs reaches
    /// that turn, so the work is redirected instead of queued behind it.
    #[test]
    fn a_message_typed_mid_turn_reaches_that_turn() {
        smol::block_on(async {
            let mut history = History::new(Vec::new());
            let (mut agent, _events) = make_agent(
                ScriptedProvider::new(vec![Ok(tool_call_response("glob", "t1")), end_turn("done")]),
                &mut history,
            );
            let queue = InterruptQueue::new();
            queue.push(typed("actually, rename it"));
            agent = agent.with_interrupt_source(Arc::new(queue.clone()));

            agent.run(default_input()).await.unwrap();
            drop(agent);

            assert!(
                has_interrupt_in_history(history.as_slice()),
                "the typed message must land inside the running turn"
            );
        });
    }
}
