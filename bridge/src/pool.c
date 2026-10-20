/* waywallen-bridge — pool dispatcher.
 *
 * Owns:
 *   - drm_fd + release timeline drm_syncobj (creation, export, destroy)
 *   - bind_generation + per-slot release_point bookkeeping
 *   - ready / release_syncobj / format_caps / bind_buffers /
 *     frame_ready / bind_failed wire emission
 *   - dispatch into backend ops (pool_egl_gbm.c or pool_vulkan.c)
 *
 * Backends own:
 *   - GPU device handle (GBM device / VkDevice borrowed)
 *   - per-slot resource allocation (gbm_bo / VkImage)
 *   - per-slot handle export to the plugin (GL FBO / VkImage)
 *   - modifier probe (against the producer GPU only)
 */
#include <waywallen-bridge/bridge.h>
#include <waywallen-bridge/pool.h>

#include "log_internal.h"
#include "pool_internal.h"
#include "sync_release.h"

#include <errno.h>
#include <fcntl.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

/* -----------------------------------------------------------------------
 * Helpers
 * ----------------------------------------------------------------------- */

static int init_release_sync(ww_pool_t* p) {
    int rc = pthread_mutex_init(&p->release_mutex, NULL);
    if (rc != 0) return -rc;
    p->release_sync_initialized = true;
    return 0;
}

static void destroy_release_sync(ww_pool_t* p) {
    if (! p->release_sync_initialized) return;
    pthread_mutex_destroy(&p->release_mutex);
    p->release_sync_initialized = false;
}

static void clear_latest_content_locked(ww_pool_t* p) {
    if (p->latest_acquire_sync_fd >= 0) close(p->latest_acquire_sync_fd);
    p->latest_acquire_sync_fd = -1;
    p->latest_bind_generation = 0;
    p->latest_slot_index      = 0;
    p->latest_content_valid   = false;
}

static void reset_release_state_locked(ww_pool_t* p) {
    memset(p->last_release_point, 0, sizeof(p->last_release_point));
    memset(&p->pending_acquire, 0, sizeof(p->pending_acquire));
    memset(&p->pending_submit, 0, sizeof(p->pending_submit));
    clear_latest_content_locked(p);
    p->release_slot_count  = 0;
    p->next_slot           = 0;
    p->next_acquire_serial = 1;
}

static void close_slot_fds(ww_pool_t* p) {
    for (uint32_t i = 0; i < p->n_slots && i < WW_POOL_MAX_SLOTS; ++i) {
        for (uint32_t pl = 0; pl < WW_POOL_MAX_PLANES; ++pl) {
            if (p->slots[i].fds[pl] >= 0) {
                close(p->slots[i].fds[pl]);
                p->slots[i].fds[pl] = -1;
            }
        }
    }
    p->n_slots = 0;
}

static void init_slot_fds_unset(ww_pool_t* p) {
    for (uint32_t i = 0; i < WW_POOL_MAX_SLOTS; ++i) {
        for (uint32_t pl = 0; pl < WW_POOL_MAX_PLANES; ++pl) {
            p->slots[i].fds[pl] = -1;
        }
    }
}

static void teardown_slots(ww_pool_t* p) {
    if (p->ops && p->ops->free_slot) {
        for (uint32_t i = 0; i < p->n_slots; ++i) {
            p->ops->free_slot(p, i);
        }
    }
    close_slot_fds(p);
    memset(p->slots, 0, sizeof(p->slots));
    init_slot_fds_unset(p);
    p->n_slots = 0;
    /* Slot identities changed — back-pressure points reference the
     * old buffers. Reset so the next render frame doesn't wait on a
     * syncobj point that pertains to a destroyed buffer. */
    pthread_mutex_lock(&p->release_mutex);
    reset_release_state_locked(p);
    pthread_mutex_unlock(&p->release_mutex);
}

static int send_ready_once(ww_pool_t* p, int sock) {
    if (p->ready_sent) return 0;
    const waywallen_drm_node_t drm_node = {
        .major = p->caps.drm_render_major,
        .minor = p->caps.drm_render_minor,
    };
    int rc = ww_bridge_send_ready(sock, &drm_node);
    if (rc != 0) return rc;
    p->ready_sent = true;
    return 0;
}

static int send_release_syncobj_once(ww_pool_t* p, int sock) {
    if (p->release_syncobj_sent) return 0;

    int fd = -1;
    int rc = ww_drm_syncobj_export_fd(p->drm_fd, p->release_syncobj_handle, &fd);
    if (rc != 0) {
        ww_bridge_logf(
            WW_BRIDGE_LOG_ERROR, "ww_pool: HANDLE_TO_FD on release_syncobj failed: %d", rc);
        return rc;
    }
    rc = ww_bridge_send_release_syncobj(sock, fd);
    close(fd);
    if (rc != 0) {
        ww_bridge_logf(WW_BRIDGE_LOG_ERROR, "ww_pool: send release_syncobj failed: %d", rc);
        return rc;
    }
    p->release_syncobj_sent = true;
    return 0;
}

static int emit_bind_buffers(ww_pool_t* p, int sock) {
    if (p->n_slots == 0 || p->n_slots > WW_POOL_MAX_SLOTS) return -EINVAL;
    pthread_mutex_lock(&p->release_mutex);
    p->bind_generation += 1;
    uint64_t bind_generation = p->bind_generation;
    p->release_slot_count    = p->n_slots;
    pthread_mutex_unlock(&p->release_mutex);

    /* Every slot agrees on plane_count after alloc — the directive
     * pinned a single (fourcc, modifier) so the backend produces
     * identical layouts. Pick slot 0's plane_count as the wire's
     * `planes_per_buffer`. */
    uint32_t planes_per_buffer = p->slots[0].plane_count;
    if (planes_per_buffer == 0 || planes_per_buffer > WW_POOL_MAX_PLANES) {
        return -EINVAL;
    }
    for (uint32_t i = 1; i < p->n_slots; ++i) {
        if (p->slots[i].plane_count != planes_per_buffer) {
            ww_bridge_logf(WW_BRIDGE_LOG_ERROR,
                           "ww_pool: emit_bind_buffers: slot[%u].plane_count=%u "
                           "differs from slot[0].plane_count=%u — backend bug",
                           i,
                           p->slots[i].plane_count,
                           planes_per_buffer);
            return -EINVAL;
        }
    }

    uint32_t total = p->n_slots * planes_per_buffer;
    uint32_t strides[WW_POOL_MAX_SLOTS * WW_POOL_MAX_PLANES];
    uint32_t offsets[WW_POOL_MAX_SLOTS * WW_POOL_MAX_PLANES];
    uint64_t sizes[WW_POOL_MAX_SLOTS * WW_POOL_MAX_PLANES];
    int      fds[WW_POOL_MAX_SLOTS * WW_POOL_MAX_PLANES];
    for (uint32_t s = 0; s < p->n_slots; ++s) {
        for (uint32_t pl = 0; pl < planes_per_buffer; ++pl) {
            uint32_t flat = s * planes_per_buffer + pl;
            strides[flat] = p->slots[s].strides[pl];
            offsets[flat] = p->slots[s].plane_offsets[pl];
            sizes[flat]   = p->slots[s].sizes[pl];
            fds[flat]     = p->slots[s].fds[pl];
        }
    }

    /* Mirror the BUF_HOST_VISIBLE bit when memory source is LINEAR
     * or DMABUF_HEAP — both are GTT/sysmem-backed and PRIME-importable
     * by foreign GPUs. GPU_NATIVE may or may not be device-local;
     * leave the bit clear there (consumer treats absent as "device
     * local; same-GPU only"). */
    uint32_t bb_flags = 0;
    if (p->cur.directive.memory_source == WAYWALLEN_BUFFER_MEMORY_SOURCE_GPU_LINEAR ||
        p->cur.directive.memory_source == WAYWALLEN_BUFFER_MEMORY_SOURCE_DMABUF_HEAP) {
        bb_flags |= WW_BUF_HOST_VISIBLE;
    }

    waywallen_buffer_pool_t bb = {
        .generation = bind_generation,
        .flags      = bb_flags,
        .count      = p->n_slots,
        .format     = {
            .fourcc      = p->cur.directive.format.fourcc,
            .modifier    = p->slots[0].modifier,
            .plane_count = planes_per_buffer,
        },
        .extent       = p->cur.extent,
        .stride       = { .count = total, .data = strides },
        .plane_offset = { .count = total, .data = offsets },
        .size          = { .count = total, .data = sizes },
    };

    ww_bridge_logf(WW_BRIDGE_LOG_DEBUG,
                   "ww_pool: emit_bind_buffers gen=%llu count=%u planes=%u fourcc=0x%08x "
                   "%ux%u mod=0x%016llx flags=0x%x",
                   (unsigned long long)bb.generation,
                   bb.count,
                   planes_per_buffer,
                   bb.format.fourcc,
                   bb.extent.width,
                   bb.extent.height,
                   (unsigned long long)bb.format.modifier,
                   bb.flags);
    for (uint32_t s = 0; s < p->n_slots; ++s) {
        for (uint32_t pl = 0; pl < planes_per_buffer; ++pl) {
            uint32_t flat = s * planes_per_buffer + pl;
            ww_bridge_logf(WW_BRIDGE_LOG_DEBUG,
                           "ww_pool:   buf[%u].plane[%u] fd=%d stride=%u offset=%u size=%llu",
                           s,
                           pl,
                           fds[flat],
                           strides[flat],
                           offsets[flat],
                           (unsigned long long)sizes[flat]);
        }
    }

    return ww_bridge_send_bind_buffers(sock, &bb, fds);
}

static int validate_directive(const ww_pool_t* p, const ww_pool_directive_t* d) {
    if (! d) return -EINVAL;
    if (d->count == 0 || d->count > WW_POOL_MAX_SLOTS) return -EINVAL;
    /* Legacy directives use the advertised extent. Target transactions
     * validate their explicit extent before allocation. */
    if (p->probe_width == 0 || p->probe_height == 0) return -EINVAL;

    switch (d->path) {
    case WAYWALLEN_BUFFER_PATH_OPTIMIZED_SAME_DEVICE:
    case WAYWALLEN_BUFFER_PATH_OPTIMIZED_SAME_VENDOR:
    case WAYWALLEN_BUFFER_PATH_COMPAT_LINEAR: break;
    case WAYWALLEN_BUFFER_PATH_COMPAT_CPU_READBACK: return -ENOTSUP;
    default: return -EINVAL;
    }

    switch (d->memory_source) {
    case WAYWALLEN_BUFFER_MEMORY_SOURCE_GPU_NATIVE:
    case WAYWALLEN_BUFFER_MEMORY_SOURCE_GPU_LINEAR: break;
    case WAYWALLEN_BUFFER_MEMORY_SOURCE_DMABUF_HEAP: return -ENOTSUP;
    default: return -EINVAL;
    }

    /* OPTIMIZED paths must reference an advertised (fourcc, modifier).
     * COMPAT_LINEAR doesn't have to be advertised — bridge may
     * re-allocate a brand-new LINEAR buffer regardless. */
    if (d->path == WAYWALLEN_BUFFER_PATH_OPTIMIZED_SAME_DEVICE ||
        d->path == WAYWALLEN_BUFFER_PATH_OPTIMIZED_SAME_VENDOR) {
        bool ok = false;
        for (size_t i = 0; i < p->caps.count; ++i) {
            if (p->caps.entries[i].fourcc == d->format.fourcc &&
                p->caps.entries[i].modifier == d->format.modifier) {
                ok = true;
                break;
            }
        }
        if (! ok) return -ENOTSUP;
    }

    /* The pool lifecycle is defined by a timeline drm_syncobj. */
    if (d->sync_mode != WW_SYNC_SYNCOBJ_TIMELINE) {
        return -ENOTSUP;
    }

    return 0;
}

static void send_bind_failed_quiet(ww_pool_t* p, int sock, const waywallen_buffer_format_t* format,
                                   waywallen_buffer_allocation_failure_kind_t kind,
                                   const char*                                msg) {
    waywallen_bind_failure_t failure = {
        .format  = *format,
        .kind    = kind,
        .message = (char*)msg,
    };
    int rc = ww_bridge_send_bind_failed(sock, &failure);
    if (rc != 0) {
        ww_bridge_logf(WW_BRIDGE_LOG_ERROR, "ww_pool: send bind_failed failed: %d", rc);
    }
    (void)p;
}

/* -----------------------------------------------------------------------
 * Public API
 * ----------------------------------------------------------------------- */

int ww_bridge_pool_create(ww_pool_backend_t backend, const void* init_data, ww_pool_t** out_pool) {
    if (! init_data || ! out_pool) return -EINVAL;
    *out_pool = NULL;

    ww_pool_t* p = (ww_pool_t*)calloc(1, sizeof(*p));
    if (! p) return -ENOMEM;
    p->backend                = backend;
    p->drm_fd                 = -1;
    p->latest_acquire_sync_fd = -1;
    init_slot_fds_unset(p);

    int rc = init_release_sync(p);
    if (rc != 0) {
        free(p);
        return rc;
    }

    switch (backend) {
    case WW_POOL_BACKEND_EGL_GBM: rc = ww_pool_egl_gbm_create(p, init_data); break;
    case WW_POOL_BACKEND_VULKAN: rc = ww_pool_vulkan_create(p, init_data); break;
    default: rc = -EINVAL; break;
    }
    if (rc != 0) {
        destroy_release_sync(p);
        free(p);
        return rc;
    }

    /* Backend init must have populated drm_fd. Create the timeline
     * drm_syncobj here so apply_directive can immediately publish a
     * release_point of 1 on the first frame. */
    if (p->drm_fd < 0) {
        if (p->ops && p->ops->destroy) p->ops->destroy(p);
        destroy_release_sync(p);
        free(p);
        return -ENODEV;
    }
    rc = ww_drm_syncobj_create(p->drm_fd, &p->release_syncobj_handle);
    if (rc != 0) {
        if (p->ops && p->ops->destroy) p->ops->destroy(p);
        if (p->drm_fd >= 0) close(p->drm_fd);
        destroy_release_sync(p);
        free(p);
        return rc;
    }

    *out_pool = p;
    return 0;
}

void ww_bridge_pool_destroy(ww_pool_t* pool) {
    if (! pool) return;
    ww_bridge_pool_abort_target(pool);
    pthread_mutex_lock(&pool->release_mutex);
    pool->session_lost = true;
    pthread_mutex_unlock(&pool->release_mutex);
    /* Drain in-flight GPU work referencing any slot before tearing it
     * down. Without this, vkDestroyImage / glDeleteTextures may run on
     * still-busy resources, and any acquire dma_fence the producer has
     * exported (and the consumer is waiting on) gets force-cancelled by
     * the kernel, which on NVIDIA propagates as VK_ERROR_DEVICE_LOST on
     * the consumer side. */
    if (pool->ops && pool->ops->wait_idle) {
        pool->ops->wait_idle(pool);
    }
    teardown_slots(pool);
    if (pool->ops && pool->ops->destroy) {
        pool->ops->destroy(pool);
    }
    if (pool->release_syncobj_handle != 0 && pool->drm_fd >= 0) {
        ww_drm_syncobj_destroy(pool->drm_fd, pool->release_syncobj_handle);
    }
    if (pool->drm_fd >= 0) {
        close(pool->drm_fd);
    }
    if (pool->caps.entries) {
        free(pool->caps.entries);
    }
    destroy_release_sync(pool);
    free(pool);
}

int ww_bridge_pool_advertise_caps(ww_pool_t* pool, int sock, uint32_t width, uint32_t height,
                                  uint32_t mem_hints) {
    if (! pool) return -EINVAL;
    pool->probe_width    = width;
    pool->probe_height   = height;
    pool->caps.mem_hints = mem_hints;

    int rc = pool->ops->probe_caps(pool, width, height);
    if (rc != 0) return rc;
    pool->caps_advertised = true;

    rc = send_ready_once(pool, sock);
    if (rc != 0) return rc;
    rc = send_release_syncobj_once(pool, sock);
    if (rc != 0) return rc;

    /* Flatten the per-format entries into the protocol capability arrays. */
    if (pool->caps.count == 0) return -ENOTSUP;

    /* Worst-case scratch sizing: one fourcc per entry. */
    size_t    n                    = pool->caps.count;
    uint32_t* scratch_fourccs      = (uint32_t*)calloc(n, sizeof(uint32_t));
    uint32_t* scratch_mod_counts   = (uint32_t*)calloc(n, sizeof(uint32_t));
    uint64_t* scratch_modifiers    = (uint64_t*)calloc(n, sizeof(uint64_t));
    uint32_t* scratch_plane_counts = (uint32_t*)calloc(n, sizeof(uint32_t));
    if (! scratch_fourccs || ! scratch_mod_counts || ! scratch_modifiers ||
        ! scratch_plane_counts) {
        free(scratch_fourccs);
        free(scratch_mod_counts);
        free(scratch_modifiers);
        free(scratch_plane_counts);
        return -ENOMEM;
    }

    ww_negotiation_state_t neg = { 0 };
    neg.advertised             = pool->caps.entries;
    neg.advertised_count       = pool->caps.count;
    neg.fourcc                 = pool->caps.entries[0].fourcc;
    neg.modifier               = pool->caps.entries[0].modifier;
    neg.plane_count            = pool->caps.entries[0].plane_count;

    waywallen_producer_capabilities_t out = { 0 };
    ww_bridge_negotiation_fill_format_caps(
        &neg, scratch_fourccs, scratch_mod_counts, scratch_modifiers, scratch_plane_counts, &out);

    uint32_t device_uuid[4] = { 0 };
    uint32_t driver_uuid[4] = { 0 };
    if (pool->caps.have_uuid) {
        memcpy(device_uuid, pool->caps.device_uuid, sizeof(device_uuid));
        memcpy(driver_uuid, pool->caps.driver_uuid, sizeof(driver_uuid));
    }
    out.device_uuid = (ww_array_u32_t) { .count = 4, .data = device_uuid };
    out.driver_uuid = (ww_array_u32_t) { .count = 4, .data = driver_uuid };
    out.drm_node    = (waywallen_drm_node_t) {
        .major = pool->caps.drm_render_major,
        .minor = pool->caps.drm_render_minor,
    };
    out.mem_hints  = pool->caps.mem_hints;
    out.sync_caps  = pool->caps.sync_caps;
    out.color_caps = pool->caps.color_caps;
    out.max_extent = (waywallen_extent_t) {
        .width  = pool->caps.extent_max_w,
        .height = pool->caps.extent_max_h,
    };

    rc = ww_bridge_send_format_caps(sock, &out);
    free(scratch_fourccs);
    free(scratch_mod_counts);
    free(scratch_modifiers);
    free(scratch_plane_counts);
    return rc;
}

int ww_bridge_pool_apply_directive(ww_pool_t* pool, int sock,
                                   const ww_pool_directive_t* directive) {
    if (! pool || ! directive) return -EINVAL;
    if (! pool->caps_advertised) return -EINVAL;
    if (pool->candidate) return -EBUSY;

    pthread_mutex_lock(&pool->release_mutex);
    bool transaction_active = pool->pending_acquire.active || pool->pending_submit.active;
    pthread_mutex_unlock(&pool->release_mutex);
    if (transaction_active) return -EBUSY;

    int rc = validate_directive(pool, directive);
    if (rc != 0) {
        send_bind_failed_quiet(pool,
                               sock,
                               &directive->format,
                               WAYWALLEN_BUFFER_ALLOCATION_FAILURE_KIND_UNSUPPORTED,
                               "directive rejected by pool");
        return rc;
    }

    /* Tear down existing slots before re-allocating. */
    teardown_slots(pool);
    pool->cur.directive = *directive;
    /* Legacy negotiation uses the renderer's already-resolved extent. */
    pool->cur.extent = (waywallen_extent_t) {
        .width  = pool->probe_width,
        .height = pool->probe_height,
    };
    pool->has_directive = true;

    /* Dry-run: try slot 0 first. */
    rc = pool->ops->alloc_slot(pool, 0, &pool->slots[0]);
    if (rc != 0) {
        ww_bridge_logf(WW_BRIDGE_LOG_WARN,
                       "ww_pool: dry-run alloc_slot[0] failed (path=%u mem_src=%u "
                       "modifier=0x%016llx): %d",
                       directive->path,
                       directive->memory_source,
                       (unsigned long long)directive->format.modifier,
                       rc);
        send_bind_failed_quiet(pool,
                               sock,
                               &directive->format,
                               WAYWALLEN_BUFFER_ALLOCATION_FAILURE_KIND_ALLOCATOR_REJECTED,
                               "alloc_slot dry-run failed");
        pool->n_slots = 0;
        return rc;
    }
    pool->n_slots = 1;

    /* Allocate the rest. Failure of any later slot rolls back to a
     * full bind_failed (the daemon can't safely use a partial pool). */
    for (uint32_t i = 1; i < directive->count; ++i) {
        rc = pool->ops->alloc_slot(pool, i, &pool->slots[i]);
        if (rc != 0) {
            ww_bridge_logf(WW_BRIDGE_LOG_ERROR, "ww_pool: alloc_slot[%u] failed: %d", i, rc);
            send_bind_failed_quiet(pool,
                                   sock,
                                   &directive->format,
                                   WAYWALLEN_BUFFER_ALLOCATION_FAILURE_KIND_RESOURCE_EXHAUSTED,
                                   "alloc_slot failed mid-pool");
            teardown_slots(pool);
            return rc;
        }
        pool->n_slots = i + 1;
    }

    /* All slots allocated — emit bind_buffers. */
    rc = emit_bind_buffers(pool, sock);
    if (rc != 0) {
        ww_bridge_logf(WW_BRIDGE_LOG_ERROR, "ww_pool: emit bind_buffers failed: %d", rc);
        return rc;
    }
    return 0;
}

static int populate_slot_view(ww_pool_t* pool, uint32_t slot_index, ww_pool_slot_t* out_slot) {
    memset(out_slot, 0, sizeof(*out_slot));
    out_slot->index  = slot_index;
    out_slot->width  = pool->cur.extent.width;
    out_slot->height = pool->cur.extent.height;
    /* Plugin-facing convenience: expose plane 0 only. Plugins that
     * need multi-plane layout (rare — render targets are normally
     * GPU-internal) read the bridge's caps directly. */
    out_slot->stride       = pool->slots[slot_index].strides[0];
    out_slot->plane_offset = pool->slots[slot_index].plane_offsets[0];
    out_slot->size         = (uint32_t)pool->slots[slot_index].sizes[0];
    /* Backend fills its handle fields. */
    return pool->ops->populate_slot_view(pool, slot_index, out_slot);
}

void ww_bridge_pool_abort_target(ww_pool_t* pool) {
    if (! pool || ! pool->candidate) return;
    ww_pool_t* candidate = pool->candidate;
    if (candidate->ops->wait_idle) candidate->ops->wait_idle(candidate);
    teardown_slots(candidate);
    candidate->ops->destroy(candidate);
    destroy_release_sync(candidate);
    free(candidate);
    pool->candidate = NULL;
}

int ww_bridge_pool_prepare_target(ww_pool_t* pool, const ww_pool_directive_t* directive,
                                  uint32_t width, uint32_t height) {
    if (! pool || ! directive || ! pool->caps_advertised || ! width || ! height) return -EINVAL;
    if (! pool->ops->fork_allocation) return -ENOTSUP;
    if (pool->candidate) return -EBUSY;
    if (width > pool->caps.extent_max_w || height > pool->caps.extent_max_h) return -ERANGE;
    pthread_mutex_lock(&pool->release_mutex);
    bool busy = pool->pending_acquire.active || pool->pending_submit.active;
    bool lost = pool->session_lost;
    pthread_mutex_unlock(&pool->release_mutex);
    if (lost) return -EPIPE;
    if (busy) return -EBUSY;
    int rc = validate_directive(pool, directive);
    if (rc != 0) return rc;

    ww_pool_t* candidate = calloc(1, sizeof(*candidate));
    if (! candidate) return -ENOMEM;
    candidate->ops                    = pool->ops;
    candidate->backend                = pool->backend;
    candidate->drm_fd                 = -1;
    candidate->latest_acquire_sync_fd = -1;
    candidate->cur                    = (ww_pool_allocation_t) { *directive, { width, height } };
    candidate->has_directive          = true;
    init_slot_fds_unset(candidate);
    rc = init_release_sync(candidate);
    if (rc != 0) {
        free(candidate);
        return rc;
    }
    rc = pool->ops->fork_allocation(pool, candidate);
    if (rc != 0) {
        destroy_release_sync(candidate);
        free(candidate);
        return rc;
    }
    pool->candidate = candidate;
    for (uint32_t i = 0; i < directive->count; ++i) {
        candidate->n_slots = i + 1;
        rc                 = candidate->ops->alloc_slot(candidate, i, &candidate->slots[i]);
        if (rc != 0) {
            ww_bridge_pool_abort_target(pool);
            return rc;
        }
    }
    return 0;
}

int ww_bridge_pool_candidate_slot(ww_pool_t* pool, uint32_t index, ww_pool_slot_t* out_slot) {
    if (! pool || ! pool->candidate || ! out_slot) return -EINVAL;
    if (index >= pool->candidate->n_slots) return -ERANGE;
    return populate_slot_view(pool->candidate, index, out_slot);
}

int ww_bridge_pool_commit_target(ww_pool_t* pool, int sock, uint64_t* out_generation) {
    if (! pool || ! pool->candidate || ! out_generation) return -EINVAL;
    pthread_mutex_lock(&pool->release_mutex);
    bool busy = pool->pending_acquire.active || pool->pending_submit.active;
    bool lost = pool->session_lost;
    pthread_mutex_unlock(&pool->release_mutex);
    if (lost) return -EPIPE;
    if (busy) return -EBUSY;
    if (pool->ops->wait_idle) pool->ops->wait_idle(pool);
    ww_pool_t* candidate         = pool->candidate;
    void*      old_backend       = pool->backend_data;
    pool->backend_data           = candidate->backend_data;
    candidate->backend_data      = old_backend;
    ww_pool_allocation_t old_cur = pool->cur;
    pool->cur                    = candidate->cur;
    candidate->cur               = old_cur;
    uint32_t old_count           = pool->n_slots;
    pool->n_slots                = candidate->n_slots;
    candidate->n_slots           = old_count;
    for (uint32_t i = 0; i < WW_POOL_MAX_SLOTS; ++i) {
        ww_pool_slot_layout_t old_slot = pool->slots[i];
        pool->slots[i]                 = candidate->slots[i];
        candidate->slots[i]            = old_slot;
    }
    pool->has_directive = true;
    pthread_mutex_lock(&pool->release_mutex);
    reset_release_state_locked(pool);
    pthread_mutex_unlock(&pool->release_mutex);
    int rc = emit_bind_buffers(pool, sock);
    if (rc != 0) {
        pthread_mutex_lock(&pool->release_mutex);
        pool->session_lost = true;
        pthread_mutex_unlock(&pool->release_mutex);
    }
    *out_generation = pool->bind_generation;
    /* Consumers and PublishedPool retain their own DMA-BUF references. Retiring
     * producer handles does not reuse or overwrite that backing storage. */
    ww_bridge_pool_abort_target(pool);
    return rc;
}

static bool wait_would_block(int rc) { return rc == -ETIME || rc == -ETIMEDOUT || rc == -EAGAIN; }

static bool same_identity(const ww_pool_slot_identity_t* lhs, const ww_pool_slot_identity_t* rhs) {
    return lhs->bind_generation == rhs->bind_generation && lhs->slot_index == rhs->slot_index &&
           lhs->previous_release_point == rhs->previous_release_point &&
           lhs->acquire_serial == rhs->acquire_serial;
}

int ww_bridge_pool_get_extent(ww_pool_t* pool, uint32_t* out_width, uint32_t* out_height) {
    if (! pool || ! out_width || ! out_height) return -EINVAL;
    pthread_mutex_lock(&pool->release_mutex);
    if (! pool->has_directive || pool->n_slots == 0) {
        pthread_mutex_unlock(&pool->release_mutex);
        return -ENODATA;
    }
    *out_width  = pool->cur.extent.width;
    *out_height = pool->cur.extent.height;
    pthread_mutex_unlock(&pool->release_mutex);
    return 0;
}

int ww_bridge_pool_try_acquire_any_for_render(ww_pool_t*                     pool,
                                              ww_pool_slot_acquire_result_t* out_result) {
    if (! pool || ! out_result) return -EINVAL;

    memset(out_result, 0, sizeof(*out_result));
    out_result->status     = WW_POOL_SLOT_ACQUIRE_ERROR;
    out_result->error_code = -EINVAL;

    pthread_mutex_lock(&pool->release_mutex);
    if (pool->session_lost) {
        out_result->status     = WW_POOL_SLOT_ACQUIRE_SESSION_LOST;
        out_result->error_code = -EPIPE;
        pthread_mutex_unlock(&pool->release_mutex);
        return 0;
    }
    if (! pool->has_directive || pool->n_slots == 0 || pool->release_slot_count != pool->n_slots) {
        pthread_mutex_unlock(&pool->release_mutex);
        return 0;
    }
    if (pool->pending_acquire.active || pool->pending_submit.active) {
        out_result->error_code = -EBUSY;
        pthread_mutex_unlock(&pool->release_mutex);
        return 0;
    }

    uint32_t selected = UINT32_MAX;
    for (uint32_t offset = 0; offset < pool->n_slots; ++offset) {
        uint32_t index = (pool->next_slot + offset) % pool->n_slots;
        if (pool->last_release_point[index] == 0) {
            selected = index;
            break;
        }
    }
    if (selected == UINT32_MAX) {
        for (uint32_t offset = 0; offset < pool->n_slots; ++offset) {
            uint32_t index = (pool->next_slot + offset) % pool->n_slots;
            int      rc    = ww_drm_syncobj_timeline_wait(
                pool->drm_fd, pool->release_syncobj_handle, pool->last_release_point[index], 0);
            if (rc == 0) {
                selected = index;
                break;
            }
            if (! wait_would_block(rc)) {
                out_result->error_code = rc;
                pthread_mutex_unlock(&pool->release_mutex);
                return 0;
            }
        }
    }
    if (selected == UINT32_MAX) {
        out_result->status     = WW_POOL_SLOT_ACQUIRE_BUSY;
        out_result->error_code = 0;
        pthread_mutex_unlock(&pool->release_mutex);
        return 0;
    }

    uint64_t serial = pool->next_acquire_serial++;
    if (serial == 0) {
        out_result->error_code = -EOVERFLOW;
        pthread_mutex_unlock(&pool->release_mutex);
        return 0;
    }
    ww_pool_slot_identity_t identity = {
        .bind_generation        = pool->bind_generation,
        .slot_index             = selected,
        .previous_release_point = pool->last_release_point[selected],
        .acquire_serial         = serial,
    };
    int acquire_rc = populate_slot_view(pool, selected, &out_result->slot);
    if (acquire_rc != 0) {
        out_result->error_code = acquire_rc;
        pthread_mutex_unlock(&pool->release_mutex);
        return 0;
    }
    if (pool->latest_content_valid && pool->latest_bind_generation == pool->bind_generation &&
        pool->latest_slot_index == selected) {
        clear_latest_content_locked(pool);
    }

    pool->pending_acquire = (ww_pool_pending_acquire_t) {
        .identity = identity,
        .active   = true,
    };
    pool->next_slot      = (selected + 1) % pool->n_slots;
    out_result->identity = identity;
    out_result->status = identity.previous_release_point == 0 ? WW_POOL_SLOT_ACQUIRE_READY_UNUSED
                                                              : WW_POOL_SLOT_ACQUIRE_READY_RELEASED;
    out_result->error_code = 0;
    pthread_mutex_unlock(&pool->release_mutex);
    return 0;
}

int ww_bridge_pool_wait_acquire_any_for_render(ww_pool_t* pool, ww_pool_cancel_fn cancel,
                                               void*                          userdata,
                                               ww_pool_slot_acquire_result_t* out_result) {
    if (! pool || ! out_result) return -EINVAL;
    for (;;) {
        int rc = ww_bridge_pool_try_acquire_any_for_render(pool, out_result);
        if (rc != 0 || out_result->status != WW_POOL_SLOT_ACQUIRE_BUSY) return rc;
        if (cancel && cancel(userdata)) {
            out_result->status     = WW_POOL_SLOT_ACQUIRE_CANCELLED;
            out_result->error_code = 0;
            return 0;
        }

        pthread_mutex_lock(&pool->release_mutex);
        if (pool->session_lost) {
            out_result->status     = WW_POOL_SLOT_ACQUIRE_SESSION_LOST;
            out_result->error_code = -EPIPE;
            pthread_mutex_unlock(&pool->release_mutex);
            return 0;
        }
        uint64_t next_point = UINT64_MAX;
        for (uint32_t index = 0; index < pool->release_slot_count; ++index) {
            uint64_t point = pool->last_release_point[index];
            if (point != 0 && point < next_point) next_point = point;
        }
        pthread_mutex_unlock(&pool->release_mutex);
        if (next_point == UINT64_MAX) continue;

        rc = ww_drm_syncobj_timeline_wait(
            pool->drm_fd, pool->release_syncobj_handle, next_point, 250);
        if (rc != 0 && ! wait_would_block(rc)) {
            out_result->status     = WW_POOL_SLOT_ACQUIRE_ERROR;
            out_result->error_code = rc;
            return 0;
        }
    }
}

int ww_bridge_pool_abort_acquired_slot(ww_pool_t* pool, const ww_pool_slot_identity_t* identity) {
    if (! pool || ! identity) return -EINVAL;
    pthread_mutex_lock(&pool->release_mutex);
    if (pool->pending_submit.active) {
        pthread_mutex_unlock(&pool->release_mutex);
        return -EBUSY;
    }
    if (! pool->pending_acquire.active) {
        pthread_mutex_unlock(&pool->release_mutex);
        return -ENOENT;
    }
    if (! same_identity(&pool->pending_acquire.identity, identity)) {
        pthread_mutex_unlock(&pool->release_mutex);
        return -ESTALE;
    }
    memset(&pool->pending_acquire, 0, sizeof(pool->pending_acquire));
    pthread_mutex_unlock(&pool->release_mutex);
    return 0;
}

int ww_bridge_pool_submit_acquired_slot(ww_pool_t* pool, int sock,
                                        const ww_pool_slot_identity_t* identity,
                                        int                            acquire_sync_fd,
                                        ww_pool_slot_submit_result_t*  out_result) {
    if (! pool || ! identity || ! out_result) {
        if (acquire_sync_fd >= 0) close(acquire_sync_fd);
        return -EINVAL;
    }
    memset(out_result, 0, sizeof(*out_result));
    out_result->status     = WW_POOL_SLOT_SUBMIT_ERROR;
    out_result->error_code = -EINVAL;

    pthread_mutex_lock(&pool->release_mutex);
    out_result->identity = *identity;
    if (pool->session_lost) {
        out_result->status     = WW_POOL_SLOT_SUBMIT_SESSION_LOST;
        out_result->error_code = -EPIPE;
        pthread_mutex_unlock(&pool->release_mutex);
        if (acquire_sync_fd >= 0) close(acquire_sync_fd);
        return 0;
    }
    if (! pool->has_directive || identity->slot_index >= pool->n_slots ||
        identity->bind_generation != pool->bind_generation || ! pool->pending_acquire.active ||
        ! same_identity(&pool->pending_acquire.identity, identity) ||
        pool->last_release_point[identity->slot_index] != identity->previous_release_point) {
        out_result->error_code = -ESTALE;
        pthread_mutex_unlock(&pool->release_mutex);
        if (acquire_sync_fd >= 0) close(acquire_sync_fd);
        return 0;
    }
    if (pool->pending_submit.active) {
        out_result->error_code = -EBUSY;
        pthread_mutex_unlock(&pool->release_mutex);
        if (acquire_sync_fd >= 0) close(acquire_sync_fd);
        return 0;
    }
    if (pool->release_point == UINT64_MAX) {
        out_result->error_code = -EOVERFLOW;
        pthread_mutex_unlock(&pool->release_mutex);
        if (acquire_sync_fd >= 0) close(acquire_sync_fd);
        return 0;
    }

    int retained_sync_fd = -1;
    if (acquire_sync_fd >= 0) {
        retained_sync_fd = fcntl(acquire_sync_fd, F_DUPFD_CLOEXEC, 0);
        if (retained_sync_fd < 0) {
            out_result->error_code = -errno;
            memset(&pool->pending_acquire, 0, sizeof(pool->pending_acquire));
            pthread_mutex_unlock(&pool->release_mutex);
            close(acquire_sync_fd);
            return 0;
        }
    }

    uint64_t pt          = pool->release_point + 1;
    pool->pending_submit = (ww_pool_pending_submit_t) {
        .identity      = *identity,
        .release_point = pt,
        .active        = true,
    };
    out_result->release_point = pt;
    pthread_mutex_unlock(&pool->release_mutex);

    waywallen_frame_t fr = {
        .image_index    = identity->slot_index,
        .sequence       = pt,
        .produced_at_ns = ww_bridge_now_ns(),
        .release_point  = pt,
    };

    int rc = ww_bridge_send_frame_ready(sock, &fr, acquire_sync_fd);
    if (acquire_sync_fd >= 0) close(acquire_sync_fd);

    pthread_mutex_lock(&pool->release_mutex);
    if (rc != 0) {
        if (retained_sync_fd >= 0) close(retained_sync_fd);
        memset(&pool->pending_submit, 0, sizeof(pool->pending_submit));
        memset(&pool->pending_acquire, 0, sizeof(pool->pending_acquire));
        pool->session_lost     = true;
        out_result->status     = WW_POOL_SLOT_SUBMIT_SESSION_LOST;
        out_result->error_code = rc;
        pthread_mutex_unlock(&pool->release_mutex);
        return 0;
    }

    pool->release_point                            = pt;
    pool->last_release_point[identity->slot_index] = pt;
    if (retained_sync_fd >= 0) {
        clear_latest_content_locked(pool);
        pool->latest_acquire_sync_fd = retained_sync_fd;
        pool->latest_bind_generation = identity->bind_generation;
        pool->latest_slot_index      = identity->slot_index;
        pool->latest_content_valid   = true;
    }
    memset(&pool->pending_submit, 0, sizeof(pool->pending_submit));
    memset(&pool->pending_acquire, 0, sizeof(pool->pending_acquire));
    out_result->status     = WW_POOL_SLOT_SUBMIT_SUBMITTED;
    out_result->error_code = 0;
    pthread_mutex_unlock(&pool->release_mutex);
    return 0;
}

int ww_bridge_pool_try_republish_latest(ww_pool_t* pool, int sock,
                                        ww_pool_republish_result_t* out_result) {
    if (! pool || ! out_result) return -EINVAL;
    memset(out_result, 0, sizeof(*out_result));
    out_result->status     = WW_POOL_REPUBLISH_ERROR;
    out_result->error_code = -EINVAL;

    pthread_mutex_lock(&pool->release_mutex);
    if (pool->session_lost) {
        out_result->status     = WW_POOL_REPUBLISH_SESSION_LOST;
        out_result->error_code = -EPIPE;
        pthread_mutex_unlock(&pool->release_mutex);
        return 0;
    }
    if (! pool->has_directive || ! pool->latest_content_valid || pool->latest_acquire_sync_fd < 0 ||
        pool->latest_bind_generation != pool->bind_generation ||
        pool->latest_slot_index >= pool->n_slots) {
        out_result->status     = WW_POOL_REPUBLISH_NO_CONTENT;
        out_result->error_code = 0;
        pthread_mutex_unlock(&pool->release_mutex);
        return 0;
    }
    if (pool->pending_acquire.active || pool->pending_submit.active) {
        out_result->status     = WW_POOL_REPUBLISH_BUSY;
        out_result->error_code = -EBUSY;
        pthread_mutex_unlock(&pool->release_mutex);
        return 0;
    }

    const uint32_t slot_index = pool->latest_slot_index;
    const uint64_t point      = pool->last_release_point[slot_index];
    if (point != 0) {
        const int wait_rc =
            ww_drm_syncobj_timeline_wait(pool->drm_fd, pool->release_syncobj_handle, point, 0);
        if (wait_rc != 0) {
            out_result->status =
                wait_would_block(wait_rc) ? WW_POOL_REPUBLISH_BUSY : WW_POOL_REPUBLISH_ERROR;
            out_result->error_code = wait_would_block(wait_rc) ? 0 : wait_rc;
            pthread_mutex_unlock(&pool->release_mutex);
            return 0;
        }
    }

    const uint64_t serial = pool->next_acquire_serial++;
    if (serial == 0) {
        out_result->error_code = -EOVERFLOW;
        pthread_mutex_unlock(&pool->release_mutex);
        return 0;
    }
    const int acquire_sync_fd = fcntl(pool->latest_acquire_sync_fd, F_DUPFD_CLOEXEC, 0);
    if (acquire_sync_fd < 0) {
        out_result->error_code = -errno;
        pthread_mutex_unlock(&pool->release_mutex);
        return 0;
    }

    const ww_pool_slot_identity_t identity = {
        .bind_generation        = pool->bind_generation,
        .slot_index             = slot_index,
        .previous_release_point = point,
        .acquire_serial         = serial,
    };
    pool->pending_acquire = (ww_pool_pending_acquire_t) {
        .identity = identity,
        .active   = true,
    };
    pthread_mutex_unlock(&pool->release_mutex);

    ww_pool_slot_submit_result_t submitted;
    const int                    submit_rc =
        ww_bridge_pool_submit_acquired_slot(pool, sock, &identity, acquire_sync_fd, &submitted);
    if (submit_rc != 0) return submit_rc;

    out_result->slot_index    = slot_index;
    out_result->sequence      = submitted.release_point;
    out_result->release_point = submitted.release_point;
    switch (submitted.status) {
    case WW_POOL_SLOT_SUBMIT_SUBMITTED:
        out_result->status     = WW_POOL_REPUBLISH_PUBLISHED;
        out_result->error_code = 0;
        ww_bridge_logf(WW_BRIDGE_LOG_DEBUG,
                       "ww_pool: republished slot[%u] seq=%llu release_point=%llu",
                       slot_index,
                       (unsigned long long)out_result->sequence,
                       (unsigned long long)out_result->release_point);
        break;
    case WW_POOL_SLOT_SUBMIT_SESSION_LOST:
        out_result->status     = WW_POOL_REPUBLISH_SESSION_LOST;
        out_result->error_code = submitted.error_code;
        break;
    case WW_POOL_SLOT_SUBMIT_ERROR:
        out_result->status     = WW_POOL_REPUBLISH_ERROR;
        out_result->error_code = submitted.error_code;
        break;
    }
    return 0;
}

int ww_bridge_pool_wait_republish_latest(ww_pool_t* pool, int sock, ww_pool_cancel_fn cancel,
                                         void* userdata, ww_pool_republish_result_t* out_result) {
    if (! pool || ! out_result) return -EINVAL;
    for (;;) {
        const int rc = ww_bridge_pool_try_republish_latest(pool, sock, out_result);
        if (rc != 0 || out_result->status != WW_POOL_REPUBLISH_BUSY ||
            out_result->error_code == -EBUSY)
            return rc;
        if (cancel && cancel(userdata)) {
            out_result->status     = WW_POOL_REPUBLISH_CANCELLED;
            out_result->error_code = 0;
            return 0;
        }

        pthread_mutex_lock(&pool->release_mutex);
        if (pool->session_lost) {
            out_result->status     = WW_POOL_REPUBLISH_SESSION_LOST;
            out_result->error_code = -EPIPE;
            pthread_mutex_unlock(&pool->release_mutex);
            return 0;
        }
        const uint64_t point =
            pool->latest_content_valid && pool->latest_slot_index < pool->release_slot_count
                ? pool->last_release_point[pool->latest_slot_index]
                : 0;
        pthread_mutex_unlock(&pool->release_mutex);
        if (point == 0) continue;

        const int wait_rc =
            ww_drm_syncobj_timeline_wait(pool->drm_fd, pool->release_syncobj_handle, point, 250);
        if (wait_rc != 0 && ! wait_would_block(wait_rc)) {
            out_result->status     = WW_POOL_REPUBLISH_ERROR;
            out_result->error_code = wait_rc;
            return 0;
        }
    }
}
