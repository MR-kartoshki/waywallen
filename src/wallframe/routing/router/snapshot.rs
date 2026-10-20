use super::*;

pub(super) fn project_link(
    link: &Link,
    pool: &PublishedPool,
    info: &DisplayInfo,
    config_generation: u64,
    buffer_generation: u64,
    layout: &ResolvedLayout,
    reference: Option<crate::wallframe::renderer_manager::render_target::Size>,
) -> CompositionConfig {
    if let crate::wallframe::routing::table::LinkProjection::Canvas {
        extent,
        member,
        layout: canvas_layout,
        ..
    } = &link.projection
    {
        let projection = crate::wallframe::display::placement::project_canvas(
            pool.width,
            pool.height,
            info.metrics.width,
            info.metrics.height,
            *extent,
            *member,
            *canvas_layout,
            link.clear_rgba,
        );
        return match projection {
            Some(projection) => CompositionConfig {
                generation: config_generation,
                buffer_generation,
                display_w: info.metrics.width as f32,
                display_h: info.metrics.height as f32,
                source_x: projection.source.x,
                source_y: projection.source.y,
                source_w: projection.source.w,
                source_h: projection.source.h,
                dest_x: projection.dest.x,
                dest_y: projection.dest.y,
                dest_w: projection.dest.w,
                dest_h: projection.dest.h,
                transform: projection.transform,
                clear_rgba: link.clear_rgba,
            },
            None => CompositionConfig {
                generation: config_generation,
                buffer_generation,
                display_w: info.metrics.width as f32,
                display_h: info.metrics.height as f32,
                source_x: 0.0,
                source_y: 0.0,
                source_w: 0.0,
                source_h: 0.0,
                dest_x: 0.0,
                dest_y: 0.0,
                dest_w: 0.0,
                dest_h: 0.0,
                transform: canvas_layout.transform(),
                clear_rgba: link.clear_rgba,
            },
        };
    }

    let source = reference.map_or(link.src_rect, |reference| {
        crate::wallframe::display::placement::rescale_source(
            link.src_rect,
            crate::wallframe::display::placement::CanvasSize {
                width: reference.width,
                height: reference.height,
            },
            crate::wallframe::display::placement::CanvasSize {
                width: pool.width,
                height: pool.height,
            },
        )
    });
    let projection = crate::wallframe::display::placement::project_surface(
        crate::wallframe::display::placement::CanvasSize {
            width: pool.width,
            height: pool.height,
        },
        crate::wallframe::display::placement::CanvasSize {
            width: info.metrics.width,
            height: info.metrics.height,
        },
        source,
        link.dst_rect,
        *layout,
        link.clear_rgba,
    );
    CompositionConfig {
        generation: config_generation,
        buffer_generation,
        display_w: info.metrics.width as f32,
        display_h: info.metrics.height as f32,
        source_x: projection.source.x,
        source_y: projection.source.y,
        source_w: projection.source.w,
        source_h: projection.source.h,
        dest_x: projection.dest.x,
        dest_y: projection.dest.y,
        dest_w: projection.dest.w,
        dest_h: projection.dest.h,
        transform: projection.transform,
        clear_rgba: link.clear_rgba,
    }
}
