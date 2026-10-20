use std::collections::HashMap;

use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::wallframe::renderer_manager::RendererId;
use crate::wallframe::scheduler::DisplayId;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum DeadlineKey {
    RendererStart(RendererId),
    RenderTarget(RendererId, u64),
    AutoReplayResume(super::auto_replay::Source),
}

impl DeadlineKey {
    pub fn renderer_start(renderer_id: &str) -> Self {
        Self::RendererStart(renderer_id.to_owned())
    }

    pub fn auto_replay_resume(display_id: DisplayId) -> Self {
        Self::AutoReplayResume(super::auto_replay::Source::Display(display_id))
    }
}

#[derive(Debug)]
pub(super) struct DeadlineReached {
    pub key: DeadlineKey,
    pub token: u64,
}

enum Command {
    Schedule {
        key: DeadlineKey,
        token: u64,
        at: Instant,
    },
    Cancel {
        key: DeadlineKey,
    },
}

#[derive(Clone)]
pub(super) struct DeadlineScheduler {
    tx: mpsc::UnboundedSender<Command>,
}

impl DeadlineScheduler {
    pub fn start() -> (Self, mpsc::UnboundedReceiver<DeadlineReached>) {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        tokio::spawn(run(command_rx, event_tx));
        (Self { tx: command_tx }, event_rx)
    }

    pub fn schedule(&self, key: DeadlineKey, token: u64, at: Instant) {
        let _ = self.tx.send(Command::Schedule { key, token, at });
    }

    pub fn cancel(&self, key: DeadlineKey) {
        let _ = self.tx.send(Command::Cancel { key });
    }
}

async fn run(
    mut commands: mpsc::UnboundedReceiver<Command>,
    events: mpsc::UnboundedSender<DeadlineReached>,
) {
    let mut scheduled: HashMap<DeadlineKey, (u64, Instant)> = HashMap::new();
    loop {
        let next = scheduled.values().map(|(_, at)| *at).min();
        match next {
            Some(at) => {
                tokio::select! {
                    command = commands.recv() => {
                        let Some(command) = command else { return };
                        apply_command(&mut scheduled, command);
                    }
                    _ = tokio::time::sleep_until(at) => {
                        let now = Instant::now();
                        let due = scheduled
                            .iter()
                            .filter_map(|(key, (token, at))| {
                                (*at <= now).then(|| (key.clone(), *token))
                            })
                            .collect::<Vec<_>>();
                        for (key, token) in due {
                            scheduled.remove(&key);
                            if events.send(DeadlineReached { key, token }).is_err() {
                                return;
                            }
                        }
                    }
                }
            }
            None => {
                let Some(command) = commands.recv().await else {
                    return;
                };
                apply_command(&mut scheduled, command);
            }
        }
    }
}

fn apply_command(scheduled: &mut HashMap<DeadlineKey, (u64, Instant)>, command: Command) {
    match command {
        Command::Schedule { key, token, at } => {
            scheduled.insert(key, (token, at));
        }
        Command::Cancel { key } => {
            scheduled.remove(&key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test(start_paused = true)]
    async fn replacement_invalidates_the_previous_deadline() {
        let (scheduler, mut events) = DeadlineScheduler::start();
        let key = DeadlineKey::renderer_start("renderer");
        scheduler.schedule(key.clone(), 1, Instant::now() + Duration::from_secs(1));
        scheduler.schedule(key.clone(), 2, Instant::now() + Duration::from_secs(2));

        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(events.try_recv().is_err());
        tokio::time::advance(Duration::from_secs(1)).await;
        let reached = events.recv().await.unwrap();
        assert_eq!(reached.key, key);
        assert_eq!(reached.token, 2);
    }

    #[tokio::test(start_paused = true)]
    async fn cancellation_produces_no_event() {
        let (scheduler, mut events) = DeadlineScheduler::start();
        let key = DeadlineKey::renderer_start("renderer");
        scheduler.schedule(key.clone(), 1, Instant::now() + Duration::from_secs(1));
        scheduler.cancel(key);

        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(events.try_recv().is_err());
    }
}
