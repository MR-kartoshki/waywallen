use crate::settings::{AutoAction, AutoCondition, AutoReplayPolicy, AutoScope};
use crate::wallframe::scheduler::DisplayRole;
use tokio::time::Instant;

pub const FLAG_NON_MINIMIZED: u32 = 1 << 0;
pub const FLAG_ACTIVE: u32 = 1 << 1;
pub const FLAG_MAXIMIZED: u32 = 1 << 2;
pub const FLAG_FULLSCREEN: u32 = 1 << 3;
pub const FLAGS_KNOWN: u32 = FLAG_NON_MINIMIZED | FLAG_ACTIVE | FLAG_MAXIMIZED | FLAG_FULLSCREEN;

#[derive(Debug, Clone, Copy)]
pub struct Facts {
    pub flags: u32,
    pub session_locked: bool,
    pub session_inactive: bool,
    pub gamemode: bool,
    pub on_battery: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    Display(u64),
    Global,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Effects {
    pub pause: bool,
    pub pause_all: bool,
    pub stop: bool,
    pub stop_local: bool,
    pub mute: bool,
}

impl Effects {
    pub fn merge(&mut self, other: Self) {
        self.pause |= other.pause;
        self.pause_all |= other.pause_all;
        self.stop |= other.stop;
        self.stop_local |= other.stop_local;
        self.mute |= other.mute;
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Contribution {
    action: AutoAction,
    scope: AutoScope,
}

#[derive(Debug, Clone, Copy, Default)]
struct RuleState {
    requested: Contribution,
    resume_at: Option<Instant>,
}

/// Each condition retains its own release deadline and contribution.
#[derive(Debug, Default)]
pub struct State {
    pub last_flags: u32,
    pub stop_applied: bool,
    rules: [RuleState; AutoCondition::ALL.len()],
    pub resume_token: u64,
}

impl State {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(
        &mut self,
        policy: &AutoReplayPolicy,
        facts: Facts,
        global: bool,
        now: Instant,
        reset: bool,
    ) {
        self.last_flags = facts.flags;
        for (condition, state) in AutoCondition::ALL.into_iter().zip(&mut self.rules) {
            let contribution =
                if condition.is_global() == global && condition_matches(condition, facts) {
                    Contribution {
                        action: policy.action_for(condition),
                        scope: policy.scope_for(condition),
                    }
                } else {
                    Contribution::default()
                };
            if reset || contribution.action != AutoAction::None {
                state.requested = contribution;
                state.resume_at = None;
            } else if state.requested.action != AutoAction::None {
                let at = *state.resume_at.get_or_insert(
                    now + std::time::Duration::from_millis(u64::from(
                        policy.effective_resume_delay_ms(),
                    )),
                );
                if now >= at {
                    state.requested = Contribution::default();
                    state.resume_at = None;
                }
            }
        }
        self.resume_token = self
            .resume_token
            .checked_add(1)
            .expect("auto replay token exhausted");
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        self.rules.iter().filter_map(|r| r.resume_at).min()
    }

    pub fn effects(&self) -> Effects {
        self.effects_for(DisplayRole::Desktop)
    }

    pub fn effects_for(&self, role: DisplayRole) -> Effects {
        let mut effects = Effects::default();
        for (condition, rule) in AutoCondition::ALL.into_iter().zip(self.rules) {
            if role == DisplayRole::LockScreen
                && rule.requested.action == AutoAction::Pause
                && matches!(
                    condition,
                    AutoCondition::AnyWindow
                        | AutoCondition::Focused
                        | AutoCondition::Maximized
                        | AutoCondition::Fullscreen
                        | AutoCondition::SessionLocked
                )
            {
                continue;
            }
            match rule.requested.action {
                AutoAction::None => {}
                AutoAction::Pause if rule.requested.scope == AutoScope::CurrentDisplay => {
                    effects.pause = true
                }
                AutoAction::Pause => effects.pause_all = true,
                AutoAction::Stop if rule.requested.scope == AutoScope::CurrentDisplay => {
                    effects.stop_local = true
                }
                AutoAction::Stop => effects.stop = true,
                AutoAction::Mute => effects.mute = true,
            }
        }
        effects
    }
}

fn condition_matches(condition: AutoCondition, facts: Facts) -> bool {
    let has = |b: u32| facts.flags & b != 0;
    match condition {
        AutoCondition::AnyWindow => has(FLAG_NON_MINIMIZED),
        AutoCondition::Focused => has(FLAG_ACTIVE),
        AutoCondition::Maximized => has(FLAG_MAXIMIZED),
        AutoCondition::Fullscreen => has(FLAG_FULLSCREEN),
        AutoCondition::SessionLocked => facts.session_locked,
        AutoCondition::SessionInactive => facts.session_inactive,
        AutoCondition::GameMode => facts.gamemode,
        AutoCondition::OnBattery => facts.on_battery,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(flags: u32) -> Facts {
        Facts {
            flags,
            session_locked: false,
            session_inactive: false,
            gamemode: false,
            on_battery: false,
        }
    }

    #[test]
    fn lock_screen_filters_pause_sources_without_losing_other_actions_or_deadlines() {
        let now = Instant::now();
        let policy = AutoReplayPolicy {
            session_locked: AutoAction::Pause,
            session_inactive: AutoAction::Pause,
            resume_delay_ms: 100,
            ..Default::default()
        };
        let mut state = State::new();
        let mut input = facts(0);
        input.session_locked = true;
        state.update(&policy, input, true, now, false);
        assert!(state.effects().pause_all);
        assert!(!state.effects_for(DisplayRole::LockScreen).pause_all);
        input.session_inactive = true;
        state.update(&policy, input, true, now, false);
        assert!(state.effects_for(DisplayRole::LockScreen).pause_all);
        input.session_inactive = false;
        state.update(&policy, input, true, now, false);
        assert!(state.effects_for(DisplayRole::LockScreen).pause_all);
        state.update(
            &policy,
            input,
            true,
            now + std::time::Duration::from_millis(100),
            false,
        );
        assert!(!state.effects_for(DisplayRole::LockScreen).pause_all);
        assert!(state.effects().pause_all);
        for action in [AutoAction::Stop, AutoAction::Mute] {
            let mut policy = policy.clone();
            policy.session_locked = action;
            state.update(&policy, input, true, now, true);
            assert_eq!(state.effects_for(DisplayRole::LockScreen), state.effects());
        }
        let mut policy = policy;
        policy.fullscreen_scope = AutoScope::AllDisplays;
        state.update(&policy, facts(FLAG_FULLSCREEN), false, now, true);
        assert!(state.effects().pause_all);
        assert_eq!(
            state.effects_for(DisplayRole::LockScreen),
            Effects::default()
        );
    }

    #[test]
    fn simultaneous_conditions_preserve_mute_and_pause() {
        let policy = AutoReplayPolicy {
            focused: AutoAction::Mute,
            ..Default::default()
        };
        let mut state = State::new();
        state.update(
            &policy,
            facts(FLAG_ACTIVE | FLAG_FULLSCREEN),
            false,
            Instant::now(),
            false,
        );
        assert_eq!(
            state.effects(),
            Effects {
                pause: true,
                mute: true,
                ..Default::default()
            }
        );
    }

    #[test]
    fn display_and_global_conditions_have_separate_owners() {
        let policy = AutoReplayPolicy {
            fullscreen_scope: AutoScope::AllDisplays,
            ..Default::default()
        };
        let mut state = State::new();
        let mut input = facts(FLAG_FULLSCREEN);
        input.session_locked = true;
        state.update(&policy, input, false, Instant::now(), false);
        assert_eq!(
            state.effects(),
            Effects {
                pause_all: true,
                ..Default::default()
            }
        );
        state.update(&policy, input, true, Instant::now(), true);
        assert_eq!(
            state.effects(),
            Effects {
                stop: true,
                ..Default::default()
            }
        );
    }

    #[test]
    fn gamemode_contributes_only_to_global_state() {
        let policy = AutoReplayPolicy {
            gamemode: AutoAction::Stop,
            ..Default::default()
        };
        let mut state = State::new();
        let mut input = facts(0);
        input.gamemode = true;

        state.update(&policy, input, false, Instant::now(), false);
        assert_eq!(state.effects(), Effects::default());

        state.update(&policy, input, true, Instant::now(), true);
        assert_eq!(
            state.effects(),
            Effects {
                stop: true,
                ..Default::default()
            }
        );
    }

    #[test]
    fn conditions_release_independently_and_retrigger_cancels_deadline() {
        let policy = AutoReplayPolicy {
            focused: AutoAction::Mute,
            resume_delay_ms: 100,
            ..Default::default()
        };
        let mut state = State::new();
        let now = Instant::now();
        state.update(
            &policy,
            facts(FLAG_ACTIVE | FLAG_FULLSCREEN),
            false,
            now,
            false,
        );
        state.update(&policy, facts(FLAG_ACTIVE), false, now, false);
        assert!(state.effects().pause && state.effects().mute);
        let later = now + std::time::Duration::from_millis(100);
        state.update(&policy, facts(FLAG_ACTIVE), false, later, false);
        assert!(!state.effects().pause && state.effects().mute);
        state.update(&policy, facts(0), false, later, false);
        state.update(&policy, facts(FLAG_ACTIVE), false, later, false);
        assert!(state.next_deadline().is_none());
        assert!(state.effects().mute);
    }

    #[test]
    fn local_stop_retains_its_scope_until_resume_deadline() {
        let mut state = State::new();
        let policy = AutoReplayPolicy {
            fullscreen: AutoAction::Stop,
            fullscreen_scope: AutoScope::CurrentDisplay,
            resume_delay_ms: 100,
            ..Default::default()
        };
        let now = Instant::now();
        state.update(&policy, facts(FLAG_FULLSCREEN), false, now, false);
        assert!(state.effects().stop_local);
        assert!(!state.effects().stop);
        state.update(&policy, facts(0), false, now, false);
        assert!(state.effects().stop_local);
        state.update(
            &policy,
            facts(0),
            false,
            now + std::time::Duration::from_millis(100),
            false,
        );
        assert!(!state.effects().stop_local);
    }

    #[test]
    fn settings_reset_removes_old_delayed_contributions() {
        let mut state = State::new();
        let now = Instant::now();
        state.update(
            &AutoReplayPolicy::default(),
            facts(FLAG_FULLSCREEN),
            false,
            now,
            false,
        );
        let policy = AutoReplayPolicy {
            fullscreen: AutoAction::None,
            ..Default::default()
        };
        state.update(&policy, facts(FLAG_FULLSCREEN), false, now, true);
        assert_eq!(state.effects(), Effects::default());
        assert!(state.next_deadline().is_none());
    }
}
