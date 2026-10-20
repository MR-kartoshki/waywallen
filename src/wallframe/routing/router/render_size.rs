use super::*;
use crate::wallframe::display::placement::{self, CanvasSize};
use crate::wallframe::renderer_manager::render_target::{Allocation, Size};
use crate::wallframe::routing::table::LinkProjection;

impl Router {
    pub(super) async fn reconcile_render_targets(self: &Arc<Self>) {
        let _serial = self.render_size_lock.lock().await;
        let work = {
            let inner = self.inner.lock().await;
            inner
                .table
                .renderer_ids()
                .into_iter()
                .filter_map(|id| {
                    let handle = inner.table.get_renderer(&id)?;
                    if !handle.automatic_resolution() {
                        return None;
                    }
                    let desired = (|| {
                        let caps = handle.render_target_capabilities()?;
                        let producer = handle.format_caps()?;
                        let texture = CanvasSize {
                            width: caps.reference.width,
                            height: caps.reference.height,
                        };
                        let mut density = 0.0_f64;
                        let mut centered = false;
                        let mut consumers = Vec::new();
                        for link in inner.table.links_for_renderer(&id) {
                            let Some(display) = inner.displays.get(&link.display_id) else {
                                continue;
                            };
                            let surface = CanvasSize {
                                width: display.info.metrics.width,
                                height: display.info.metrics.height,
                            };
                            if surface.width == 0 || surface.height == 0 {
                                continue;
                            }
                            consumers.push(display.consumer_caps.clone());
                            let (projection, layout) = match link.projection {
                                LinkProjection::Independent => {
                                    let layout = self.resolved_layout_for_renderer(
                                        &display.info,
                                        &id,
                                        &inner,
                                    );
                                    (
                                        Some(placement::project_surface(
                                            texture,
                                            surface,
                                            link.src_rect,
                                            link.dst_rect,
                                            layout,
                                            link.clear_rgba,
                                        )),
                                        layout,
                                    )
                                }
                                LinkProjection::Canvas {
                                    extent,
                                    member,
                                    layout,
                                    ..
                                } => (
                                    placement::project_canvas(
                                        texture.width,
                                        texture.height,
                                        surface.width,
                                        surface.height,
                                        extent,
                                        member,
                                        layout,
                                        link.clear_rgba,
                                    ),
                                    layout,
                                ),
                            };
                            if let Some(scale) = projection.and_then(|projection| {
                                placement::sampling_scale(projection, surface)
                            }) {
                                centered |= layout.fillmode == FillMode::Centered;
                                density = density.max(if layout.fillmode == FillMode::Centered {
                                    1.0
                                } else {
                                    scale
                                });
                            }
                        }
                        let consumer =
                            crate::wallframe::dma::negotiate::common_consumers(&consumers)?;
                        // A connected surface showing only the background still needs a valid pool.
                        let size = match caps.select(
                            density.max(f64::EPSILON),
                            Size {
                                width: producer.extent_max.0.min(consumer.extent_max.0),
                                height: producer.extent_max.1.min(consumer.extent_max.1),
                            },
                        ) {
                            Ok(size) => size,
                            Err(error) => {
                                handle.report_render_target_problem(format!(
                                    "auto_extent_invalid: {error}"
                                ));
                                return None;
                            }
                        };
                        if centered && size != caps.reference {
                            handle.report_render_target_problem(
                                "auto_centered_requires_native_extent".into(),
                            );
                            return None;
                        }
                        let mut scheme =
                            match crate::wallframe::dma::negotiate::pick(&producer, &consumer) {
                                Ok(scheme) => scheme,
                                Err(error) => {
                                    handle.report_render_target_problem(format!(
                                        "auto_no_buffer_scheme: {error:?}"
                                    ));
                                    return None;
                                }
                            };
                        if handle.activity_mode == RendererActivityMode::OnDemand {
                            scheme.count = 1;
                        }
                        Some(Allocation {
                            capabilities_revision: caps.revision,
                            size,
                            scheme,
                        })
                    })();
                    Some((handle, desired))
                })
                .collect::<Vec<_>>()
        };
        for (handle, desired) in work {
            let key =
                deadline::DeadlineKey::RenderTarget(handle.id.clone(), handle.process_generation);
            match self
                .mgr
                .coordinate_render_target(&handle, desired, tokio::time::Instant::now())
                .await
            {
                Ok(Some(at)) => self.deadlines.schedule(key, handle.process_generation, at),
                Ok(None) => self.deadlines.cancel(key),
                Err(error) => {
                    self.deadlines.cancel(key);
                    log::error!("renderer {}: Auto configuration failed: {error}", handle.id);
                }
            }
        }
    }
}
