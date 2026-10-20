use super::*;

impl Router {
    /// Bring `display_id`'s sent state in line with its current link
    /// target (renderer + generation). Idempotent.
    pub(super) async fn sync_display(self: &Arc<Self>, display_id: DisplayId) {
        let mut inner = self.inner.lock().await;
        if !inner.displays.contains_key(&display_id) {
            return;
        }
        // Capture one exact publication for the complete display decision.
        let display_links = inner.table.links_for_display(display_id);
        debug_assert!(
            display_links.iter().filter(|l| l.enabled).count() <= 1,
            "display {display_id} has multiple enabled links — invariant violated"
        );
        let target: Option<(Link, Arc<RendererHandle>, Arc<PublishedPool>, ContentToken)> =
            display_links.into_iter().find(|l| l.enabled).and_then(|l| {
                let renderer = inner.table.get_renderer(&l.renderer_id)?;
                let pool = renderer.published_pool()?;
                let content_token = inner.content_token_for_renderer(&l.renderer_id);
                Some((l, renderer, pool, content_token))
            });

        // When both producer and consumer have caps, only bind a snapshot
        // that satisfies the last negotiated scheme.
        if let Some((_, ref renderer, ref pool, _)) = target {
            if renderer.format_caps().is_some() && !renderer.scheme_satisfied_by(pool) {
                log::debug!(
                    "router: sync_display({display_id}) gated — renderer {} \
                     published pool does not match last-dispatched scheme",
                    renderer.id
                );
                return;
            }
        }

        // Snapshot what was last sent.
        let (last_binding, info) = {
            let s = inner.displays.get(&display_id).unwrap();
            (s.binding.clone(), s.info.clone())
        };

        let needs_update = match (&last_binding, &target) {
            (Some(old), Some((link, _, pool, content_token))) => {
                old.renderer.id != link.renderer_id
                    || old.pool.generation != pool.generation
                    || old.content_token != *content_token
            }
            (None, None) => false,
            _ => true,
        };
        if !needs_update {
            return;
        }

        inner
            .displays
            .get(&display_id)
            .expect("display checked above")
            .invalidate_consumption();

        // Retire the prior pool if one was bound.
        if let Some(old) = last_binding.as_ref() {
            let s = inner.displays.get(&display_id).unwrap();
            let _ = s.tx.send(DisplayOutEvent::Unbind {
                buffer_generation: old.wire_generation,
            });
            // If the OLD renderer is currently being torn down with
            // ack tracking active, record this unbind as pending.
            if let Some(pending) = inner.unbind_acks_pending.get_mut(&old.renderer.id) {
                pending.insert((display_id, old.wire_generation));
            }
        }

        // Bind the new pool if a target renderer is ready.
        if let Some((link, renderer, pool, content_token)) = target {
            inner.next_config_generation += 1;
            let cfg_gen = inner.next_config_generation;
            let layout = self.resolved_layout_for_renderer(&info, &link.renderer_id, &inner);
            let replay = renderer
                .wp_type
                .eq_ignore_ascii_case("image")
                .then(|| renderer.latest_frame())
                .flatten()
                .filter(|frame| frame.buffer_generation == pool.generation);
            let presentation_config_generation = inner
                .displays
                .get(&display_id)
                .unwrap()
                .presentation
                .config
                .generation;
            let s = inner.displays.get_mut(&display_id).unwrap();
            s.next_wire_buffer_generation = s
                .next_wire_buffer_generation
                .checked_add(1)
                .expect("display buffer generation exhausted");
            let wire_generation = s.next_wire_buffer_generation;
            let reference = renderer
                .render_target_capabilities()
                .map(|caps| caps.reference);
            let cfg = project_link(
                &link,
                &pool,
                &info,
                cfg_gen,
                wire_generation,
                &layout,
                reference,
            );
            let _ = s.tx.send(DisplayOutEvent::Bind {
                renderer: renderer.clone(),
                pool: Arc::clone(&pool),
                buffer_generation: wire_generation,
                initial_config: cfg,
                content_token,
                presentation_config_generation,
            });
            let replayed = replay.is_some_and(|frame| {
                s.tx.send(DisplayOutEvent::Frame {
                    renderer: renderer.clone(),
                    buffer_generation: wire_generation,
                    buffer_index: frame.buffer_index,
                    seq: frame.seq,
                    consumption: s.consumption_permit(),
                    member: None,
                })
                .is_ok()
            });
            let last_frame_seq = replay.filter(|_| replayed).map(|frame| frame.seq);
            s.binding = Some(DisplayBinding {
                renderer,
                pool,
                wire_generation,
                content_token,
                awaiting_initial_frame: !replayed,
                last_frame_seq,
            });
            s.failed_binding_generation = None;
        } else {
            let s = inner.displays.get_mut(&display_id).unwrap();
            s.binding = None;
        }
    }
}
