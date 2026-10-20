use super::*;

impl Router {
    pub async fn update_window_observation(
        self: &Arc<Self>,
        display_id: DisplayId,
        generation: u64,
        flags: u32,
    ) -> Result<(), &'static str> {
        {
            let mut inner = self.inner.lock().await;
            let state = inner
                .displays
                .get_mut(&display_id)
                .ok_or("display is disconnected")?;
            let rules = self.resolved_auto_replay(&state.info).window_exclusions;
            if let Some(config) = state.window_observation.reconcile(&rules) {
                let _ = state
                    .tx
                    .send(DisplayOutEvent::SetWindowObservationConfig(config));
            }
            if !state.window_observation.accept(generation)? {
                return Ok(());
            }
            state.auto_replay.last_flags = state.info.role.window_flags(flags);
        }
        self.refresh_auto_policy(false).await;
        Ok(())
    }
    pub async fn update_session_state(
        self: &Arc<Self>,
        locked: Option<bool>,
        inactive: Option<bool>,
    ) {
        {
            let mut inner = self.inner.lock().await;
            if let Some(locked) = locked {
                inner.session_locked = locked;
            }
            if let Some(inactive) = inactive {
                inner.session_inactive = inactive;
            }
        }
        self.refresh_auto_policy(false).await;
    }

    pub async fn update_gamemode_state(self: &Arc<Self>, active: bool) {
        self.inner.lock().await.gamemode = active;
        self.refresh_auto_policy(false).await;
    }

    pub async fn update_power_state(self: &Arc<Self>, on_battery: bool) {
        {
            let mut inner = self.inner.lock().await;
            if inner.on_battery == on_battery {
                return;
            }
            inner.on_battery = on_battery;
        }
        log::info!("power: on_battery={on_battery}");
        self.refresh_auto_policy(false).await;
    }

    pub(super) async fn refresh_auto_policy(self: &Arc<Self>, reset: bool) {
        let mut support_changed = false;
        let changed = {
            let mut inner = self.inner.lock().await;
            let now = tokio::time::Instant::now();
            let global = self
                .settings
                .get()
                .map(|s| s.global().effective_auto_replay())
                .unwrap_or_default();
            let facts = auto_replay::Facts {
                flags: 0,
                session_locked: inner.session_locked,
                session_inactive: inner.session_inactive,
                gamemode: inner.gamemode,
                on_battery: inner.on_battery,
            };
            inner
                .global_auto_replay
                .update(&global, facts, true, now, reset);
            self.schedule_auto_resume(auto_replay::Source::Global, &inner.global_auto_replay);
            let mut effects = inner.global_auto_replay.effects();
            let mut lock_effects = inner
                .global_auto_replay
                .effects_for(DisplayRole::LockScreen);
            for (&id, state) in &mut inner.displays {
                let policy = self.resolved_auto_replay(&state.info);
                let previous_unsupported = state.window_observation.unsupported;
                if let Some(config) = state
                    .window_observation
                    .reconcile(&policy.window_exclusions)
                {
                    let _ = state
                        .tx
                        .send(DisplayOutEvent::SetWindowObservationConfig(config));
                }
                if state.window_observation.unsupported != previous_unsupported
                    && state.window_observation.unsupported != 0
                {
                    log::warn!(
                        "display {id}: window exclusions are not fully supported by this client"
                    );
                }
                support_changed |= state.window_observation.unsupported != previous_unsupported;
                state.auto_replay.update(
                    &policy,
                    auto_replay::Facts {
                        flags: state.info.role.window_flags(state.auto_replay.last_flags),
                        ..facts
                    },
                    false,
                    now,
                    reset,
                );
                self.schedule_auto_resume(auto_replay::Source::Display(id), &state.auto_replay);
                let contribution = state.auto_replay.effects();
                effects.merge(auto_replay::Effects {
                    pause: false,
                    stop_local: false,
                    ..contribution
                });
                lock_effects.merge(auto_replay::Effects {
                    pause: false,
                    stop_local: false,
                    ..state.auto_replay.effects_for(DisplayRole::LockScreen)
                });
            }
            let mut changed = inner.auto_effects != effects;
            inner.auto_effects = effects;
            for state in inner.displays.values_mut() {
                let was_paused = state.display_paused();
                let was_auto_paused = state.auto_paused;
                let applicable = match state.info.role {
                    DisplayRole::Desktop => effects,
                    DisplayRole::LockScreen => lock_effects,
                };
                state.auto_paused = !applicable.stop
                    && (applicable.pause_all
                        || state.auto_replay.effects_for(state.info.role).pause);
                state.consumption_changed(was_paused);
                changed |= was_auto_paused != state.auto_paused
                    || state.auto_replay.stop_applied
                        != (effects.stop || state.auto_replay.effects().stop_local);
            }
            changed
        };
        if changed {
            self.reconcile_lifecycle().await;
        }
        if support_changed {
            self.emit(RouterEvent::DisplaysReplace(self.snapshot_displays().await));
        }
    }

    fn schedule_auto_resume(&self, source: auto_replay::Source, state: &auto_replay::State) {
        let key = deadline::DeadlineKey::AutoReplayResume(source);
        if let Some(at) = state.next_deadline() {
            self.deadlines.schedule(key, state.resume_token, at);
        } else {
            self.deadlines.cancel(key);
        }
    }

    pub(super) async fn finish_auto_replay_resume(
        self: &Arc<Self>,
        source: auto_replay::Source,
        token: u64,
    ) {
        let current = {
            let inner = self.inner.lock().await;
            match source {
                auto_replay::Source::Global => Some(&inner.global_auto_replay),
                auto_replay::Source::Display(id) => inner.displays.get(&id).map(|s| &s.auto_replay),
            }
            .is_some_and(|s| s.resume_token == token && s.next_deadline().is_some())
        };
        if current {
            self.refresh_auto_policy(false).await;
        }
    }
}
