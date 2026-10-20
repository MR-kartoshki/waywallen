// waywallen-bridge — buffer pool + path-explicit modifier negotiation.

#ifndef WAYWALLEN_BRIDGE_POOL_H
#define WAYWALLEN_BRIDGE_POOL_H

#include <waywallen-bridge/ipc_v3.h>
#include <waywallen-bridge/protocol_bits.h>

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Daemon → bridge allocation policy. The generated protocol type is the
 * public owner of path, memory-source, format, and synchronization fields. */
typedef waywallen_buffer_directive_t ww_pool_directive_t;

/* -----------------------------------------------------------------------
 * Backend selection
 * ----------------------------------------------------------------------- */

typedef enum ww_pool_backend
{
    WW_POOL_BACKEND_EGL_GBM = 0, /* mpv plugin */
    WW_POOL_BACKEND_VULKAN  = 1, /* image plugin */
} ww_pool_backend_t;

/* EGL/GBM backend init descriptor. Plugin owns EGLDisplay + EGL
 * context — bridge only borrows them via this struct and the
 * eglGetProcAddress callback. The DRM render-node fd is *moved*
 * into the bridge: bridge wraps it in a gbm_device and closes both
 * on pool destroy.
 *
 * `egl_display` is `EGLDisplay` (an opaque handle on every Mesa
 * platform); cast in/out at the call site to keep this header
 * EGL-include-free. */
typedef struct ww_pool_egl_gbm_init {
    void* egl_display;                           /* EGLDisplay */
    int   drm_render_fd;                         /* moved */
    void* (*get_proc_address)(const char* name); /* eglGetProcAddress */
    /* DRM render major/minor as reported by EGL_DRM_RENDER_NODE_FILE_EXT
     * → stat(); used for the `ready` event sent by the bridge. Pass
     * (0,0) if unknown. */
    uint32_t drm_render_major;
    uint32_t drm_render_minor;
} ww_pool_egl_gbm_init_t;

/* Vulkan backend init descriptor. Plugin owns the VkInstance, the
 * VkPhysicalDevice it picked, and the VkDevice + queue. Bridge
 * borrows them and creates VkImage / VkDeviceMemory / VkSemaphore
 * objects on top.
 *
 * `instance` / `physical_device` / `device` are `VkInstance` /
 * `VkPhysicalDevice` / `VkDevice` (opaque handles); cast at the
 * call site to keep this header Vulkan-include-free.
 *
 * `device_uuid` and `driver_uuid` are 16-byte buffers from
 * `VkPhysicalDeviceIDProperties`; bridge ships them in `format_caps`.
 * Pass NULL on either to send 16 zero bytes. */
typedef struct ww_pool_vulkan_init {
    void*    instance;
    void*    physical_device;
    void*    device;
    uint32_t queue_family_index;
    void*    queue; /* VkQueue used for transfers */
    void* (*get_instance_proc_addr)(void* instance, const char* name);
    const uint8_t* device_uuid; /* NULL or 16 bytes */
    const uint8_t* driver_uuid; /* NULL or 16 bytes */
    /* Advisory fallback for the DRM render-node identity advertised
     * to the daemon. Bridge always tries VK_EXT_physical_device_drm
     * on the supplied physical_device first; these fields are used
     * only when the extension isn't available (older drivers).
     * Pass 0/0 to skip the fallback — empty values cause the daemon
     * to treat topology as unknown for this producer. */
    uint32_t drm_render_major;
    uint32_t drm_render_minor;
    /* Optional render-node fd for drm_syncobj timeline export. If -1,
     * bridge opens `/dev/dri/renderD<minor>` for the queried minor
     * (or falls back to first-openable when the minor isn't queryable).
     * Pass an existing fd to make bridge dup it — useful when the
     * producer already keeps the render node open and wants to share
     * its kernel object table with bridge. */
    int drm_render_fd;
    /* VkImageUsageFlags for VkImageCreateInfo.usage at slot allocation
     * time. */
    uint32_t image_usage_flags;
    /* VkFormatFeatureFlags the negotiated modifier's
     * drmFormatModifierTilingFeatures must cover.
     * Pass 0 for the TRANSFER_DST_BIT default. */
    uint32_t format_feature_flags;
} ww_pool_vulkan_init_t;

/* -----------------------------------------------------------------------
 * Opaque pool object
 * ----------------------------------------------------------------------- */

typedef struct ww_pool ww_pool_t;

#define WW_POOL_MAX_SLOT_COUNT 8u

/* Create a pool with the chosen backend. `init_data` points at an
 * `ww_pool_egl_gbm_init_t` or `ww_pool_vulkan_init_t` matching the
 * backend selector. The pool DOES NOT yet have any slots — caller
 * must follow with `advertise_caps` then react to `negotiate_buffers`
 * (which reaches `apply_directive`).
 *
 * On success: `*out_pool` non-NULL, returns 0. On failure returns a
 * negative errno; `*out_pool` is left NULL.
 *
 * Bridge takes ownership of:
 *   - `init_data->drm_render_fd` (will be close()'d in destroy).
 *   - All allocations the bridge makes inside the pool.
 *
 * Bridge does NOT take ownership of:
 *   - `egl_display`, `instance`, `physical_device`, `device`, `queue`
 *     (caller's lifetime). */
int ww_bridge_pool_create(ww_pool_backend_t backend, const void* init_data, ww_pool_t** out_pool);

/* Destroy a pool. Tears down all slot resources, closes the drm_fd,
 * destroys the GBM device or Vulkan-side bridge objects, and frees
 * the pool. Safe on NULL. */
void ww_bridge_pool_destroy(ww_pool_t* pool);

/* Probe the producer's per-fourcc modifier capabilities, encode them
 * as `format_caps`, and send on `sock`. Sends `ready` first if not
 * already sent and `release_syncobj` (timeline) right after. After
 * this call the producer is fully advertised and the daemon may
 * dispatch `negotiate_buffers` at any time.
 *
 * `width` / `height` are the pixel extent the renderer wants to
 * allocate at; bridge probes each candidate modifier with this size.
 *
 * `mem_hints` is the producer's advertised memory capability set. */
int ww_bridge_pool_advertise_caps(ww_pool_t* pool, int sock, uint32_t width, uint32_t height,
                                  uint32_t mem_hints);

/* Apply a directive received via `WW_EVT_IN_NEGOTIATE_BUFFERS`. The pool
 * validates it, replaces existing slots, verifies the allocation with the
 * first slot, allocates the rest, and emits `bind_buffers` with the DMA-BUF
 * fds. An initial allocation failure emits `bind_failed` so the daemon can
 * choose another format without terminating the renderer.
 *
 * Returns 0 on success, negative on dry-run failure (bind_failed
 * already sent), positive on protocol/system error (caller should
 * shut down). */
int ww_bridge_pool_apply_directive(ww_pool_t* pool, int sock, const ww_pool_directive_t* directive);

/* Per-slot resource view returned from `acquire_slot`. Plugin renders
 * into the backend-specific handle:
 *   - EGL/GBM: bind `gl_export_fbo`, draw, glFlush, hand back.
 *   - Vulkan:  record commands targeting `vk_image`, submit, hand back.
 *
 * The slot remains owned by the bridge throughout — the plugin only
 * writes into the exposed handle. Slot index roundtrips back through
 * `submit_slot`. */
typedef struct ww_pool_slot {
    uint32_t index;
    /* EGL/GBM backend: bind these to draw. Both 0 on the Vulkan
     * backend. */
    uint32_t gl_export_fbo;
    uint32_t gl_export_texture;
    /* Vulkan backend: render into this image. Both NULL on the
     * EGL/GBM backend. */
    void* vk_image;
    void* vk_memory;
    /* Layout (informational; same across slots within one directive).
     * Plugin doesn't usually need these — bridge already filled
     * `bind_buffers` — but they're handy when the upload path needs
     * the stride (image plugin's vkCmdCopyBufferToImage). */
    uint32_t width;
    uint32_t height;
    uint32_t stride;
    uint32_t plane_offset;
    uint32_t size;
} ww_pool_slot_t;

typedef enum ww_pool_slot_acquire_status
{
    /* The slot has never been submitted in this pool generation. */
    WW_POOL_SLOT_ACQUIRE_READY_UNUSED = 0,
    /* The daemon release timeline proves the previous use completed. */
    WW_POOL_SLOT_ACQUIRE_READY_RELEASED = 1,
    /* Every slot is currently owned by a consumer. */
    WW_POOL_SLOT_ACQUIRE_BUSY = 2,
    /* Pool state, wait, or backend snapshot failed. See `error_code`. */
    WW_POOL_SLOT_ACQUIRE_ERROR = 3,
    /* The renderer transport failed; no further pool work is valid. */
    WW_POOL_SLOT_ACQUIRE_SESSION_LOST = 4,
    /* A blocking acquire was cancelled by its caller. */
    WW_POOL_SLOT_ACQUIRE_CANCELLED = 5,
} ww_pool_slot_acquire_status_t;

typedef struct ww_pool_slot_identity {
    uint64_t bind_generation;
    uint32_t slot_index;
    uint64_t previous_release_point;
    uint64_t acquire_serial;
} ww_pool_slot_identity_t;

typedef struct ww_pool_slot_acquire_result {
    ww_pool_slot_acquire_status_t status;
    /* Negative errno-style value for ERROR, otherwise zero. */
    int32_t                 error_code;
    ww_pool_slot_identity_t identity;
    ww_pool_slot_t          slot;
} ww_pool_slot_acquire_result_t;

/* Read the current pool extent without acquiring a writable slot. */
int ww_bridge_pool_get_extent(ww_pool_t* pool, uint32_t* out_width, uint32_t* out_height);

/* One unpublished allocation at a time. Prepare/commit/abort and rendering
 * must be serialized by the caller. Failure leaves the active allocation intact.
 * The caller may render into candidate slots before publishing; drain/invalidate
 * any cached GPU views before commit or abort. Device ownership is unchanged.
 * Backends without allocation transactions return -ENOTSUP. */
int  ww_bridge_pool_prepare_target(ww_pool_t* pool, const ww_pool_directive_t* directive,
                                   uint32_t width, uint32_t height);
int  ww_bridge_pool_candidate_slot(ww_pool_t* pool, uint32_t index, ww_pool_slot_t* out_slot);
int  ww_bridge_pool_commit_target(ww_pool_t* pool, int sock, uint64_t* out_generation);
void ww_bridge_pool_abort_target(ww_pool_t* pool);

/* Acquire any slot whose previous release point has been signaled.
 * READY_* starts one transaction and returns its identity and writable
 * backend handle. BUSY never exposes a handle. At most one transaction
 * may be outstanding per pool. */
int ww_bridge_pool_try_acquire_any_for_render(ww_pool_t*                     pool,
                                              ww_pool_slot_acquire_result_t* out_result);

typedef int (*ww_pool_cancel_fn)(void* userdata);

/* Wait for any slot without imposing an ownership timeout. The callback
 * is polled while waiting; a non-zero result returns CANCELLED. A NULL
 * callback waits until a slot is released or the session is lost. */
int ww_bridge_pool_wait_acquire_any_for_render(ww_pool_t* pool, ww_pool_cancel_fn cancel,
                                               void*                          userdata,
                                               ww_pool_slot_acquire_result_t* out_result);

/* Abandon an acquired slot without publishing it. */
int ww_bridge_pool_abort_acquired_slot(ww_pool_t* pool, const ww_pool_slot_identity_t* identity);

typedef enum ww_pool_slot_submit_status
{
    WW_POOL_SLOT_SUBMIT_SUBMITTED    = 0,
    WW_POOL_SLOT_SUBMIT_SESSION_LOST = 1,
    WW_POOL_SLOT_SUBMIT_ERROR        = 2,
} ww_pool_slot_submit_status_t;

typedef struct ww_pool_slot_submit_result {
    ww_pool_slot_submit_status_t status;
    int32_t                      error_code;
    ww_pool_slot_identity_t      identity;
    uint64_t                     release_point;
} ww_pool_slot_submit_result_t;

/* Publish one rendered slot transactionally. The release point remains
 * private while `frame_ready` is sent and is committed to the slot only
 * after send success. A transport failure marks the entire pool session
 * lost; subsequent acquire/submit calls return SESSION_LOST. */
int ww_bridge_pool_submit_acquired_slot(ww_pool_t* pool, int sock,
                                        const ww_pool_slot_identity_t* identity,
                                        int                            acquire_sync_fd,
                                        ww_pool_slot_submit_result_t*  out_result);

typedef enum ww_pool_republish_status
{
    WW_POOL_REPUBLISH_PUBLISHED    = 0,
    WW_POOL_REPUBLISH_NO_CONTENT   = 1,
    WW_POOL_REPUBLISH_BUSY         = 2,
    WW_POOL_REPUBLISH_CANCELLED    = 3,
    WW_POOL_REPUBLISH_SESSION_LOST = 4,
    WW_POOL_REPUBLISH_ERROR        = 5,
} ww_pool_republish_status_t;

typedef struct ww_pool_republish_result {
    ww_pool_republish_status_t status;
    int32_t                    error_code;
    uint32_t                   slot_index;
    uint64_t                   sequence;
    uint64_t                   release_point;
} ww_pool_republish_result_t;

/* Publish the most recently submitted slot again without modifying its
 * contents. The previous consumer release must complete first; the new
 * frame always receives a fresh sequence and release point. */
int ww_bridge_pool_try_republish_latest(ww_pool_t* pool, int sock,
                                        ww_pool_republish_result_t* out_result);

/* Wait for the latest slot's release and republish it. Cancellation does
 * not discard the retained content, so a later request may retry. */
int ww_bridge_pool_wait_republish_latest(ww_pool_t* pool, int sock, ww_pool_cancel_fn cancel,
                                         void* userdata, ww_pool_republish_result_t* out_result);

#ifdef __cplusplus
} /* extern "C" */
#endif

#endif /* WAYWALLEN_BRIDGE_POOL_H */
