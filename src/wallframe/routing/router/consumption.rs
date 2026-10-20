use super::*;

impl DisplayState {
    pub(super) fn display_paused(&self) -> bool {
        self.manual_paused || self.auto_paused || self.global_paused
    }

    pub(super) fn consumption_changed(&mut self, was_paused: bool) {
        if was_paused != self.display_paused() {
            self.invalidate_consumption();
            self.resume_frame_requested = !self.display_paused();
        }
    }
}

impl Inner {
    pub(super) fn set_global_pause(&mut self, paused: bool) -> bool {
        if self.manual_paused == paused {
            return false;
        }
        self.manual_paused = paused;
        for display in self.displays.values_mut() {
            let was_paused = display.display_paused();
            display.global_paused = paused && display.info.role.accepts_global_pause();
            display.consumption_changed(was_paused);
        }
        true
    }

    pub(super) fn has_frame_demand(&self, link: &Link) -> bool {
        link.enabled
            && self.displays.get(&link.display_id).is_some_and(|state| {
                !state.display_paused()
                    || state.binding.as_ref().is_none_or(|binding| {
                        binding.renderer.id != link.renderer_id || binding.awaiting_initial_frame
                    })
            })
    }

    pub(super) fn has_playback_demand(&self, link: &Link) -> bool {
        // Paused consumers can still refresh a new binding without advancing playback.
        self.has_frame_demand(link)
            && self
                .displays
                .get(&link.display_id)
                .is_some_and(|state| !state.global_paused)
    }

    pub(super) fn effective_display_paused(&self, display: &DisplayState) -> bool {
        display.display_paused()
            || self
                .table
                .links_for_display(display.info.id)
                .iter()
                .any(|link| {
                    self.renderer_manual_paused.contains(&link.renderer_id)
                        || self
                            .renderer_slots
                            .get(&link.renderer_id)
                            .is_some_and(|slot| {
                                slot.state.activity() == Some(RendererActivity::Paused)
                            })
                })
    }
}

impl Router {
    pub async fn set_display_paused(
        self: &Arc<Self>,
        display_id: DisplayId,
        paused: bool,
    ) -> crate::error::Result<DisplaySnapshot> {
        let changed = {
            let mut inner = self.inner.lock().await;
            let state = inner
                .displays
                .get_mut(&display_id)
                .ok_or(crate::error::Error::DisplayNotFound(display_id))?;
            if state.manual_paused == paused {
                false
            } else {
                let was_paused = state.display_paused();
                state.manual_paused = paused;
                state.consumption_changed(was_paused);
                true
            }
        };
        if changed {
            self.reconcile_lifecycle().await;
        }
        self.snapshot_display(display_id)
            .await
            .ok_or(crate::error::Error::DisplayNotFound(display_id))
    }

    pub(super) async fn reconcile_display_consumption(self: &Arc<Self>) {
        let (refresh, changed) = {
            let mut inner = self.inner.lock().await;
            let states = inner
                .displays
                .iter()
                .map(|(&id, state)| {
                    let paused = (state.manual_paused, inner.effective_display_paused(state));
                    let targets = inner
                        .table
                        .links_for_display(id)
                        .into_iter()
                        .filter(|link| inner.has_frame_demand(link))
                        .map(|link| link.renderer_id)
                        .collect::<Vec<_>>();
                    (id, paused, targets)
                })
                .collect::<Vec<_>>();
            let mut refresh = HashSet::new();
            let mut changed = Vec::new();
            for (id, paused, targets) in states {
                let state = inner.displays.get_mut(&id).unwrap();
                if state.resume_frame_requested && !targets.is_empty() {
                    state.resume_frame_requested = false;
                    refresh.extend(targets);
                }
                if state.published_pause != Some(paused) {
                    state.published_pause = Some(paused);
                    changed.push(id);
                }
            }
            (refresh, changed)
        };
        for renderer_id in refresh {
            if let Err(error) = self.mgr.request_frame(&renderer_id).await {
                log::warn!("request current frame from renderer {renderer_id} after display resume: {error}");
            }
        }
        for display_id in changed {
            if let Some(snapshot) = self.snapshot_display(display_id).await {
                self.emit(RouterEvent::DisplayUpsert(snapshot));
            }
        }
    }
}
