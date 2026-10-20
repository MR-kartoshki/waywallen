use super::*;

pub(super) fn run_reader(
    id: RendererId,
    process_generation: RendererProcessGeneration,
    read_stream: StdUnixStream,
    writer: RendererWriter,
    subscriptions: Arc<RendererSubscriptionRegistry>,
    events: broadcast::Sender<RendererEvent>,
    published_pool: Arc<StdMutex<Option<Arc<PublishedPool>>>>,
    sync_fds: Arc<StdMutex<std::collections::VecDeque<(u64, OwnedFd)>>>,
    latest_frame: Arc<StdMutex<Option<FrameSnapshot>>>,
    release_syncobj: Arc<StdMutex<Option<OwnedFd>>>,
    format_caps: Arc<StdMutex<Option<crate::wallframe::dma::negotiate::PeerCaps>>>,
    render_target: Arc<StdMutex<render_target::State>>,
    pending_configure: Arc<StdMutex<Option<u32>>>,
    reported_state: Arc<StdMutex<RendererReportedState>>,
    progress: Arc<StdMutex<RendererProgress>>,
    reap_tx: tokio::sync::mpsc::UnboundedSender<(RendererId, RendererProcessGeneration)>,
) {
    // Any reader exit enqueues renderer eviction so stale ids do not remain
    // registered after EOF, recvmsg error, or panic.
    let _reap = ReaperOnDrop {
        id: id.clone(),
        process_generation,
        tx: reap_tx,
    };

    loop {
        let received = match recv_event(&read_stream) {
            Ok(ok) => ok,
            Err(e) => {
                log::info!("renderer {id}: reader exit: {e}");
                return;
            }
        };
        let (msg, fds) = received;
        let mut event_pool_generation = None;
        let mut state_changed_fields = 0;

        if let EventMsg::SetEventSubscriptions { ref subscription } = msg {
            let applied = subscriptions.prepare(&id, subscription.revision, &subscription.kinds);
            let ack = ControlMsg::EventSubscriptionsApplied {
                result: EventSubscriptionResult {
                    revision: applied.revision,
                    status: applied.status,
                    kinds: applied.kinds,
                    reason: applied.reason,
                },
            };
            if let Err(error) = writer.send_blocking(ack, applied.commit) {
                log::warn!("renderer {id}: subscription acknowledgement failed: {error}");
                return;
            }
        }

        // Cache each BindBuffers snapshot with its fds; later generations
        // replace earlier ones.
        if let EventMsg::BindBuffers { ref pool } = msg {
            let generation = pool.generation;
            let flags = pool.flags;
            let count = pool.count;
            let fourcc = pool.format.fourcc;
            let modifier = pool.format.modifier;
            let planes_per_buffer = pool.format.plane_count;
            let width = pool.extent.width;
            let height = pool.extent.height;
            let stride = &pool.stride;
            let plane_offset = &pool.plane_offset;
            let size = &pool.size;
            // Validate parallel arrays up front so all per-plane fields
            // stay index-aligned.
            let expected = (count as usize) * (planes_per_buffer as usize);
            if stride.len() != expected
                || plane_offset.len() != expected
                || size.len() != expected
                || fds.len() != expected
            {
                log::warn!(
                    "renderer {id}: BindBuffers length mismatch \
                     count={count} planes={planes_per_buffer} expected={expected} \
                     stride={} offset={} size={} fds={}; dropping",
                    stride.len(),
                    plane_offset.len(),
                    size.len(),
                    fds.len()
                );
            } else if fds.is_empty() {
                log::warn!("renderer {id}: BindBuffers arrived without fds");
            } else {
                let prev_gen = published_pool
                    .lock()
                    .ok()
                    .and_then(|g| g.as_ref().map(|s| s.generation));
                if let Some(prev) = prev_gen {
                    if generation <= prev {
                        log::warn!(
                            "renderer {id}: BindBuffers gen={generation} not > prev {prev}; \
                             accepting anyway but display protocol expects monotonicity"
                        );
                    }
                }
                let pool = Arc::new(PublishedPool {
                    generation,
                    flags,
                    count,
                    fourcc,
                    width,
                    height,
                    modifier,
                    planes_per_buffer,
                    stride: stride.clone(),
                    plane_offset: plane_offset.clone(),
                    size: size.clone(),
                    fds,
                });
                if let Ok(mut guard) = published_pool.lock() {
                    *guard = Some(pool);
                    event_pool_generation = Some(generation);
                    log::info!(
                        "renderer {id}: BindBuffers cached (gen={generation}, flags=0x{flags:x})"
                    );
                }
                if let Ok(mut state) = progress.lock() {
                    state.bind_at = Some(Instant::now());
                    state.buffer_generation = Some(generation);
                    state.first_frame_at = None;
                    state.last_frame_at = None;
                }
                // A rebind retires acquire fences from the previous
                // buffer_generation.
                if let Ok(mut guard) = sync_fds.lock() {
                    guard.clear();
                }
                if let Ok(mut guard) = latest_frame.lock() {
                    *guard = None;
                }
                // Clear any in-flight ConfigureBuffers, warning if the
                // renderer answered with different flags.
                if let Ok(mut guard) = pending_configure.lock() {
                    if let Some(want) = guard.take() {
                        if want != flags {
                            log::warn!(
                                "renderer {id}: ConfigureBuffers asked for \
                                 flags=0x{want:x} but renderer answered \
                                 with flags=0x{flags:x}; accepting"
                            );
                        }
                    }
                }
            }
        } else if let EventMsg::FrameReady { ref frame } = msg {
            // frame_ready always carries exactly one sync_fd: the codec
            // enforced expected_fds() == 1 before handing us `fds`.
            let mut taken = fds;
            let fd = taken.remove(0);
            if let Ok(mut guard) = sync_fds.lock() {
                while guard.len() >= SYNC_FD_RETENTION {
                    guard.pop_front();
                }
                guard.push_back((frame.sequence, fd));
            }
            let gen = published_pool
                .lock()
                .ok()
                .and_then(|g| g.as_ref().map(|s| s.generation));
            if let Some(buffer_generation) = gen {
                event_pool_generation = Some(buffer_generation);
                if let Ok(mut state) = progress.lock() {
                    let now = Instant::now();
                    if state.buffer_generation == Some(buffer_generation) {
                        state.first_frame_at.get_or_insert(now);
                        state.last_frame_at = Some(now);
                    }
                }
                if let Ok(mut guard) = latest_frame.lock() {
                    *guard = Some(FrameSnapshot {
                        buffer_generation,
                        buffer_index: frame.image_index,
                        seq: frame.sequence,
                        release_point: frame.release_point,
                    });
                }
            }
        } else if let EventMsg::ReleaseSyncobj = msg {
            // Producer's exported timeline drm_syncobj. Exactly one fd;
            // the codec enforced expected_fds() == 1.
            let mut taken = fds;
            let fd = taken.remove(0);
            if let Ok(mut guard) = release_syncobj.lock() {
                if guard.is_some() {
                    log::warn!(
                        "renderer {id}: ReleaseSyncobj received twice; \
                         replacing previous fd"
                    );
                }
                *guard = Some(fd);
                log::info!("renderer {id}: ReleaseSyncobj imported");
            }
        } else if let EventMsg::FormatCaps { ref capabilities } = msg {
            let drm = DrmNode {
                major: capabilities.drm_node.major,
                minor: capabilities.drm_node.minor,
            };
            match crate::wallframe::dma::negotiate::unflatten_caps(
                &capabilities.fourccs,
                &capabilities.mod_counts,
                &capabilities.modifiers,
                &capabilities.plane_counts,
                &capabilities.device_uuid,
                &capabilities.driver_uuid,
                drm,
                capabilities.sync_caps,
                capabilities.color_caps,
                capabilities.mem_hints,
                (
                    capabilities.max_extent.width,
                    capabilities.max_extent.height,
                ),
            ) {
                Ok(caps) => {
                    if let Ok(mut guard) = format_caps.lock() {
                        if guard.is_some() {
                            log::warn!(
                                "renderer {id}: FormatCaps received twice; \
                                 replacing previous caps"
                            );
                        }
                        let prefix = format!("renderer {id}: format_caps");
                        log::info!(
                            "{prefix}: imported {} fourcc{}",
                            caps.formats.by_fourcc.len(),
                            if caps.formats.by_fourcc.len() == 1 {
                                ""
                            } else {
                                "s"
                            },
                        );
                        caps.log_dump(&prefix);
                        *guard = Some(caps);
                    }
                }
                Err(e) => {
                    log::warn!("renderer {id}: FormatCaps malformed: {e:?}");
                }
            }
        } else if let EventMsg::RenderTargetCapabilities { ref capabilities } = msg {
            let mut state = render_target.lock().unwrap();
            if state.policy == crate::plugin::renderer_registry::ResolutionPolicy::Auto {
                match render_target::Capabilities::try_from(capabilities.clone()) {
                    Ok(caps)
                        if state
                            .capabilities
                            .as_ref()
                            .is_none_or(|old| old.revision < caps.revision) =>
                    {
                        state.capabilities = Some(caps);
                        state.problem = None;
                    }
                    Ok(caps) => {
                        if state
                            .capabilities
                            .as_ref()
                            .is_some_and(|old| old.revision == caps.revision && *old != caps)
                        {
                            state.problem = Some("auto_capabilities_revision_conflict".into());
                            log::error!(
                                "renderer {id}: Auto capabilities changed without a new revision"
                            );
                        }
                    }
                    Err(error) => {
                        state.problem = Some(format!("auto_capabilities_invalid: {error}"));
                        log::error!("renderer {id}: Auto capabilities rejected: {error}");
                    }
                }
            }
        } else if let EventMsg::RenderTargetResult { ref result } = msg {
            let pool = published_pool.lock().unwrap().clone();
            let mut state = render_target.lock().unwrap();
            if let Some((revision, target)) = state.queue.pending().copied() {
                if revision == result.revision {
                    let success = result.status == 0
                        && result.width == target.size.width
                        && result.height == target.size.height
                        && pool.as_ref().is_some_and(|pool| {
                            pool.generation == result.pool_generation
                                && pool.width == result.width
                                && pool.height == result.height
                        });
                    state.queue.finish(revision, success);
                    if success {
                        state.applied_generation = Some(result.pool_generation);
                        state.problem = None;
                        state.retry = None;
                        log::info!(
                            "renderer {id}: render target {revision} applied {}x{} pool {}",
                            result.width,
                            result.height,
                            result.pool_generation
                        );
                    } else {
                        state.problem = Some(format!("auto_target_failed: {}", result.message));
                        if result.status == 3 {
                            if let Some(caps) = format_caps.lock().unwrap().as_mut() {
                                caps.blacklist
                                    .insert((target.scheme.fourcc, target.scheme.modifier));
                                state.retry_allocation(target);
                            }
                        }
                        log::error!(
                            "renderer {id}: render target {revision} failed: status={} {}",
                            result.status,
                            result.message
                        );
                    }
                }
            }
        } else if let EventMsg::BindFailed { ref failure } = msg {
            // Renderer-side bind failure is surfaced for debugging; router
            // retry paths handle consumer-side failures.
            log::warn!(
                "renderer {id}: BindFailed fourcc=0x{:08x} \
                 modifier=0x{:x} kind={:?} msg={:?}",
                failure.format.fourcc,
                failure.format.modifier,
                failure.kind,
                failure.message,
            );
        } else if let EventMsg::ReportState { ref state } = msg {
            match reported_state.lock() {
                Ok(mut stored) => match apply_renderer_state_patch(&mut stored, state) {
                    Ok(changed) => state_changed_fields = changed,
                    Err(error) => log::warn!("renderer {id}: invalid ReportState: {error}"),
                },
                Err(_) => {
                    log::error!("renderer {id}: reported state mutex poisoned");
                }
            }
        } else if !fds.is_empty() {
            log::warn!("renderer {id}: unexpected fds on event {msg:?}, dropping");
        }

        // Broadcast to any subscribers. No subscribers means no error:
        // SendError is only returned when receivers drop, which is fine.
        let _ = events.send(RendererEvent {
            message: msg,
            pool_generation: event_pool_generation,
            state_changed_fields,
        });
    }
}

// ---------------------------------------------------------------------------
// Helpers

/// RAII guard that enqueues the renderer for eviction when the reader
/// thread drops on any exit path.
struct ReaperOnDrop {
    id: RendererId,
    process_generation: RendererProcessGeneration,
    tx: tokio::sync::mpsc::UnboundedSender<(RendererId, RendererProcessGeneration)>,
}

impl Drop for ReaperOnDrop {
    fn drop(&mut self) {
        let id = std::mem::take(&mut self.id);
        let _ = self.tx.send((id, self.process_generation));
    }
}
