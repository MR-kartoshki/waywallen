use std::time::Duration;

use tokio::time::Instant;

use crate::wallframe::ipc::proto::RenderTargetCapabilities;

pub const RESIZE_DELAY: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Allocation {
    pub capabilities_revision: u64,
    pub size: Size,
    pub scheme: crate::wallframe::dma::negotiate::NegotiatedScheme,
}

#[derive(Default)]
pub(super) struct State {
    pub policy: crate::plugin::renderer_registry::ResolutionPolicy,
    pub capabilities: Option<Capabilities>,
    pub queue: Coordinator<Allocation>,
    pub applied_generation: Option<u64>,
    pub problem: Option<String>,
    pub demand_problem: Option<String>,
    pub retry: Option<(Size, u64)>,
    pub retries: u8,
    pub(super) blocked: Option<(Size, u64)>,
}

impl State {
    pub(super) fn retry_allocation(&mut self, target: Allocation) -> bool {
        let key = (target.size, target.capabilities_revision);
        if self.retries >= 2 {
            self.blocked = Some(key);
            self.retry = None;
            return false;
        }
        self.retries += 1;
        self.retry = Some(key);
        true
    }
}

impl super::RendererHandle {
    #[cfg(test)]
    pub(crate) fn test_auto_resolution(&self, reference: Size) {
        let mut state = self.render_target.lock().unwrap();
        state.policy = crate::plugin::renderer_registry::ResolutionPolicy::Auto;
        state.capabilities = Some(Capabilities {
            native_extent: true,
            revision: 1,
            reference,
            min: Size {
                width: 1,
                height: 1,
            },
            max: reference,
            alignment: Size {
                width: 1,
                height: 1,
            },
        });
    }

    #[cfg(test)]
    pub(crate) fn test_ack_render_target(&self, revision: u64) {
        assert!(self
            .render_target
            .lock()
            .unwrap()
            .queue
            .finish(revision, true));
    }

    pub fn automatic_resolution(&self) -> bool {
        self.render_target.lock().unwrap().policy
            == crate::plugin::renderer_registry::ResolutionPolicy::Auto
    }

    pub fn render_target_capabilities(&self) -> Option<Capabilities> {
        self.render_target.lock().unwrap().capabilities.clone()
    }

    pub fn retry_render_target_import(&self, generation: u64) -> bool {
        let mut state = self.render_target.lock().unwrap();
        if state.policy != crate::plugin::renderer_registry::ResolutionPolicy::Auto {
            return true;
        }
        if state.applied_generation != Some(generation) {
            return false;
        }
        let Some(target) = state.queue.applied().copied() else {
            return false;
        };
        state.retry_allocation(target)
    }

    pub fn report_render_target_problem(&self, problem: String) {
        let mut state = self.render_target.lock().unwrap();
        if state.demand_problem.as_ref() != Some(&problem) {
            log::error!("renderer {}: {problem}", self.id);
            state.demand_problem = Some(problem);
        }
    }

    pub fn render_target_problem(&self) -> Option<String> {
        let mut state = self.render_target.lock().unwrap();
        if state.policy != crate::plugin::renderer_registry::ResolutionPolicy::Auto {
            return None;
        }
        if state.problem.is_none() {
            if state.capabilities.is_none()
                && self.progress().registered_at.elapsed() >= RESIZE_DELAY
            {
                state.problem = Some("auto_capabilities_timeout".into());
            } else if state.queue.pending().is_some()
                && state
                    .queue
                    .last_sent
                    .is_some_and(|at| at.elapsed() >= Duration::from_secs(30))
            {
                state.problem = Some("auto_request_timeout".into());
            }
            if let Some(problem) = &state.problem {
                log::error!("renderer {}: {problem}", self.id);
            }
        }
        state
            .demand_problem
            .clone()
            .or_else(|| state.problem.clone())
    }
}

impl super::RendererManager {
    pub async fn coordinate_render_target(
        &self,
        handle: &super::RendererHandle,
        desired: Option<Allocation>,
        now: Instant,
    ) -> crate::error::Result<Option<Instant>> {
        let due = {
            let mut state = handle.render_target.lock().unwrap();
            if let Some(target) = desired {
                if state.policy != crate::plugin::renderer_registry::ResolutionPolicy::Auto
                    || state
                        .capabilities
                        .as_ref()
                        .is_none_or(|caps| caps.revision != target.capabilities_revision)
                {
                    return Err(crate::error::Error::InvalidArgument(
                        "renderer did not declare the requested Auto capability revision".into(),
                    ));
                }
                state.demand_problem = None;
                if state.blocked == Some((target.size, target.capabilities_revision)) {
                    return Ok(None);
                }
                state.blocked = None;
                if state.queue.desired.as_ref().is_some_and(|(old, _)| {
                    old.size != target.size
                        || old.capabilities_revision != target.capabilities_revision
                }) {
                    state.retries = 0;
                }
            }
            let retry = state.retry.take().is_some_and(|(size, revision)| {
                desired.is_some_and(|target| {
                    target.size == size && target.capabilities_revision == revision
                })
            });
            if state.queue.desire(desired, now) {
                log::debug!(
                    "renderer {}: Auto candidate {:?}",
                    handle.id,
                    desired.map(|target| target.size)
                );
            }
            if retry {
                state.queue.take_retry(now)
            } else {
                state.queue.take_due(now)
            }
        };
        if let Some((revision, target)) = due {
            let directive = super::buffer_directive(target.scheme);
            let config = crate::wallframe::ipc::proto::RenderTargetConfig {
                revision,
                capabilities_revision: target.capabilities_revision,
                width: target.size.width,
                height: target.size.height,
                fourcc: directive.format.fourcc,
                modifier: directive.format.modifier,
                plane_count: directive.format.plane_count,
                sync_mode: directive.sync_mode,
                color: directive.color,
                mem_hint: directive.mem_hint,
                count: directive.count,
                path: directive.path,
                memory_source: directive.memory_source,
            };
            log::info!(
                "renderer {}: render target request {revision}: {}x{}",
                handle.id,
                target.size.width,
                target.size.height
            );
            if let Err(error) = self
                .send_control_to_handle(
                    &handle.id,
                    handle,
                    crate::wallframe::ipc::proto::ControlMsg::ConfigureRenderTarget { config },
                )
                .await
            {
                handle
                    .render_target
                    .lock()
                    .unwrap()
                    .queue
                    .finish(revision, false);
                return Err(error);
            }
        }
        Ok(handle.render_target.lock().unwrap().queue.deadline())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Size {
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Capabilities {
    native_extent: bool,
    pub revision: u64,
    pub reference: Size,
    min: Size,
    max: Size,
    alignment: Size,
}

impl TryFrom<RenderTargetCapabilities> for Capabilities {
    type Error = &'static str;

    fn try_from(value: RenderTargetCapabilities) -> Result<Self, Self::Error> {
        if !matches!(value.mode, 1 | 2) || value.revision == 0 {
            return Err("unsupported render target mode or revision");
        }
        let result = Self {
            native_extent: value.mode == 1,
            revision: value.revision,
            reference: Size {
                width: value.reference_width,
                height: value.reference_height,
            },
            min: Size {
                width: value.min_width,
                height: value.min_height,
            },
            max: Size {
                width: value.max_width,
                height: value.max_height,
            },
            alignment: Size {
                width: value.width_alignment,
                height: value.height_alignment,
            },
        };
        for (reference, min, max, alignment) in [
            (
                result.reference.width,
                result.min.width,
                result.max.width,
                result.alignment.width,
            ),
            (
                result.reference.height,
                result.min.height,
                result.max.height,
                result.alignment.height,
            ),
        ] {
            if reference == 0 || min == 0 || alignment == 0 || min > max || min > reference {
                return Err("invalid render target extent limits");
            }
        }
        result.select(1.0, result.max)?;
        Ok(result)
    }
}

impl Capabilities {
    pub fn select(&self, demand: f64, hardware_max: Size) -> Result<Size, &'static str> {
        if !demand.is_finite() || demand <= 0.0 {
            return Err("invalid render target pixel density");
        }
        let reference_limit = if self.native_extent {
            self.reference
        } else if self.reference.width >= self.reference.height {
            Size {
                width: 7680,
                height: 4320,
            }
        } else {
            Size {
                width: 4320,
                height: 7680,
            }
        };
        let limit = |reference: u32, max: u32, hardware: u32, alignment: u32| {
            reference.min(max).min(hardware) / alignment * alignment
        };
        let max_width = limit(
            reference_limit.width,
            self.max.width,
            hardware_max.width,
            self.alignment.width,
        );
        let max_height = limit(
            reference_limit.height,
            self.max.height,
            hardware_max.height,
            self.alignment.height,
        );
        let lower = (self.min.width as f64 / self.reference.width as f64)
            .max(self.min.height as f64 / self.reference.height as f64);
        let upper = (max_width as f64 / self.reference.width as f64)
            .min(max_height as f64 / self.reference.height as f64);
        if lower > upper || upper <= 0.0 {
            return Err("incompatible render target extent limits");
        }
        let scale = demand.max(lower).min(upper);
        let align = |reference: u32, alignment: u32, max: u32| {
            let value = (reference as f64 * scale).ceil() as u64;
            (value.div_ceil(alignment as u64) * alignment as u64).min(max as u64) as u32
        };
        Ok(Size {
            width: align(self.reference.width, self.alignment.width, max_width),
            height: align(self.reference.height, self.alignment.height, max_height),
        })
    }
}

/// Connection-local; dropping the renderer handle drops all pending revisions.
pub struct Coordinator<T> {
    desired: Option<(T, Instant)>,
    applied: Option<T>,
    in_flight: Option<(u64, T)>,
    failed: Option<T>,
    last_sent: Option<Instant>,
    revision: u64,
}

impl<T> Default for Coordinator<T> {
    fn default() -> Self {
        Self {
            desired: None,
            applied: None,
            in_flight: None,
            failed: None,
            last_sent: None,
            revision: 0,
        }
    }
}

impl<T: Clone + PartialEq> Coordinator<T> {
    pub fn desire(&mut self, value: Option<T>, now: Instant) -> bool {
        if self.desired.as_ref().map(|(value, _)| value) == value.as_ref() {
            return false;
        }
        self.desired = value.map(|value| (value, now));
        true
    }

    pub fn deadline(&self) -> Option<Instant> {
        let (desired, since) = self.desired.as_ref()?;
        if self.in_flight.is_some()
            || self.applied.as_ref() == Some(desired)
            || self.failed.as_ref() == Some(desired)
        {
            return None;
        }
        Some(match self.last_sent {
            None => *since,
            Some(sent) => (*since + RESIZE_DELAY).max(sent + RESIZE_DELAY),
        })
    }

    pub fn take_due(&mut self, now: Instant) -> Option<(u64, T)> {
        if self.deadline()? > now {
            return None;
        }
        self.take_retry(now)
    }

    fn take_retry(&mut self, now: Instant) -> Option<(u64, T)> {
        self.deadline()?;
        let target = self.desired.as_ref()?.0.clone();
        self.revision = self.revision.checked_add(1)?;
        self.last_sent = Some(now);
        self.in_flight = Some((self.revision, target.clone()));
        Some((self.revision, target))
    }

    pub fn pending(&self) -> Option<&(u64, T)> {
        self.in_flight.as_ref()
    }
    pub fn applied(&self) -> Option<&T> {
        self.applied.as_ref()
    }

    pub fn finish(&mut self, revision: u64, success: bool) -> bool {
        if self.in_flight.as_ref().map(|(revision, _)| *revision) != Some(revision) {
            return false;
        }
        let (_, target) = self.in_flight.take().unwrap();
        if success {
            self.applied = Some(target);
            self.failed = None;
        } else {
            self.failed = Some(target);
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn allocation(width: u32) -> Allocation {
        use crate::wallframe::dma::negotiate::*;
        Allocation {
            capabilities_revision: 1,
            size: Size {
                width,
                height: width / 2,
            },
            scheme: NegotiatedScheme {
                fourcc: DRM_FORMAT_ABGR8888,
                modifier: 0,
                plane_count: 1,
                sync_mode: SYNC_SYNCOBJ_TIMELINE,
                color: DEFAULT_COLOR,
                mem_hint: 0,
                count: 1,
                path: PathCategory::CompatLinear,
                mem_source: MemSource::GpuLinear,
            },
        }
    }

    #[tokio::test]
    async fn render_target_manager_gates_legacy_and_includes_extent() {
        use crate::wallframe::{
            ipc::{proto::ControlMsg, uds::recv_control},
            renderer_manager::{RendererHandle, RendererManager},
        };
        let manager = RendererManager::new_default();
        let (handle, peer) = RendererHandle::test_stub_with_peer("auto", "image");
        handle.render_target.lock().unwrap().policy =
            crate::plugin::renderer_registry::ResolutionPolicy::Auto;
        handle.render_target.lock().unwrap().capabilities = Some(Capabilities {
            native_extent: true,
            revision: 1,
            reference: Size {
                width: 3840,
                height: 1920,
            },
            min: Size {
                width: 1,
                height: 1,
            },
            max: Size {
                width: 3840,
                height: 1920,
            },
            alignment: Size {
                width: 1,
                height: 1,
            },
        });
        peer.set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        manager.register_test_handle(handle.clone()).await;
        manager
            .send_negotiate_buffers("auto", allocation(1920).scheme)
            .await
            .unwrap();
        assert!(recv_control(&peer).is_err());
        let now = Instant::now();
        manager
            .coordinate_render_target(&handle, Some(allocation(1920)), now)
            .await
            .unwrap();
        let ControlMsg::ConfigureRenderTarget { config } = recv_control(&peer).unwrap().0 else {
            panic!("wrong control");
        };
        assert_eq!(config.width, 1920);
        assert_eq!(config.revision, 1);
        handle.render_target.lock().unwrap().queue.finish(1, true);
        assert_eq!(
            manager
                .coordinate_render_target(&handle, Some(allocation(3840)), now)
                .await
                .unwrap(),
            Some(now + RESIZE_DELAY)
        );
        manager
            .coordinate_render_target(
                &handle,
                Some(allocation(3840)),
                now + Duration::from_millis(9999),
            )
            .await
            .unwrap();
        assert!(recv_control(&peer).is_err());
        manager
            .coordinate_render_target(&handle, Some(allocation(3840)), now + RESIZE_DELAY)
            .await
            .unwrap();
        let ControlMsg::ConfigureRenderTarget { config } = recv_control(&peer).unwrap().0 else {
            panic!("wrong control");
        };
        assert_eq!(config.width, 3840);
        assert_eq!(config.revision, 2);
    }

    #[test]
    fn render_target_format_retry_does_not_restart_size_debounce() {
        let mut state = Coordinator::default();
        let now = Instant::now();
        let initial = allocation(1920);
        state.desire(Some(initial), now);
        state.take_due(now).unwrap();
        state.finish(1, false);
        let mut fallback = initial;
        fallback.scheme.fourcc += 1;
        state.desire(Some(fallback), now);
        assert_eq!(state.take_retry(now), Some((2, fallback)));
        assert_eq!(state.take_retry(now), None);
        let mut allocation_state = State::default();
        assert!(allocation_state.retry_allocation(initial));
        assert!(allocation_state.retry_allocation(initial));
        assert!(!allocation_state.retry_allocation(initial));
        assert_eq!(allocation_state.retry, None);
        assert_eq!(
            allocation_state.blocked,
            Some((initial.size, initial.capabilities_revision))
        );
    }

    #[test]
    fn render_target_debounce_and_in_flight_reversal() {
        let mut state = Coordinator::default();
        let now = Instant::now();
        state.desire(Some(1), now);
        assert_eq!(state.take_due(now), Some((1, 1)));
        assert!(state.finish(1, true));
        state.desire(Some(2), now + Duration::from_secs(1));
        state.desire(Some(2), now + Duration::from_secs(5));
        assert_eq!(state.deadline(), Some(now + Duration::from_secs(11)));
        assert_eq!(state.take_due(now + Duration::from_millis(10999)), None);
        assert_eq!(state.take_due(now + Duration::from_secs(11)), Some((2, 2)));
        state.desire(Some(1), now + Duration::from_secs(12));
        assert_eq!(state.deadline(), None);
        assert!(!state.finish(1, true));
        assert!(state.finish(2, true));
        assert_eq!(state.take_due(now + Duration::from_secs(22)), Some((3, 1)));
    }

    #[test]
    fn render_target_cancel_disconnect_and_failure_are_bounded() {
        let mut state = Coordinator::default();
        let now = Instant::now();
        state.desire(Some(1), now);
        state.take_due(now).unwrap();
        state.finish(1, true);
        state.desire(Some(2), now);
        state.desire(Some(1), now);
        assert_eq!(state.deadline(), None);
        state.desire(Some(3), now);
        state.desire(None, now);
        assert_eq!(state.deadline(), None);
        state.desire(Some(4), now);
        let (revision, _) = state.take_due(now + RESIZE_DELAY).unwrap();
        state.finish(revision, false);
        state.desire(Some(4), now + RESIZE_DELAY * 2);
        assert_eq!(state.deadline(), None);
    }

    #[test]
    fn render_target_scalable_extent_is_bounded_by_oriented_8k() {
        for (width, height, max_width, max_height) in [
            (1920, 1080, 7680, 4320),
            (1080, 1920, 4320, 7680),
            (1000, 1000, 4320, 4320),
            (10000, 1000, 7680, 768),
        ] {
            let caps = Capabilities::try_from(RenderTargetCapabilities {
                revision: 1,
                mode: 2,
                reference_width: width,
                reference_height: height,
                min_width: 1,
                min_height: 1,
                max_width: 65535,
                max_height: 65535,
                width_alignment: 1,
                height_alignment: 1,
            })
            .unwrap();
            assert_eq!(
                caps.select(
                    100.0,
                    Size {
                        width: 65535,
                        height: 65535
                    }
                )
                .unwrap(),
                Size {
                    width: max_width,
                    height: max_height
                }
            );
        }
    }

    #[test]
    fn render_target_caps_preserve_aspect_and_bound_alignment() {
        let caps = Capabilities::try_from(RenderTargetCapabilities {
            revision: 1,
            mode: 1,
            reference_width: 3840,
            reference_height: 2160,
            min_width: 2,
            min_height: 2,
            max_width: 8192,
            max_height: 8192,
            width_alignment: 2,
            height_alignment: 2,
        })
        .unwrap();
        let hardware = Size {
            width: 8192,
            height: 8192,
        };
        assert_eq!(
            caps.select(0.5, hardware).unwrap(),
            Size {
                width: 1920,
                height: 1080
            }
        );
        assert_eq!(caps.select(8.0, hardware).unwrap(), caps.reference);
        assert_eq!(
            caps.select(
                1.0,
                Size {
                    width: 1920,
                    height: 2160
                }
            )
            .unwrap(),
            Size {
                width: 1920,
                height: 1080
            }
        );
        assert!(caps.select(f64::NAN, hardware).is_err());
        assert!(caps
            .select(
                1.0,
                Size {
                    width: 1,
                    height: 1
                }
            )
            .is_err());
    }
}
