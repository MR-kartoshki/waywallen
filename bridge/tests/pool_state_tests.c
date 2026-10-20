#include "pool_internal.h"

#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

static int populate_slot_view(ww_pool_t* pool, uint32_t slot_index, ww_pool_slot_t* out_slot) {
    (void)pool;
    out_slot->gl_export_fbo = slot_index + 100;
    return 0;
}

static const struct ww_pool_backend_ops TEST_OPS = {
    .populate_slot_view = populate_slot_view,
};

static void init_pool(ww_pool_t* pool, uint32_t slot_count) {
    memset(pool, 0, sizeof(*pool));
    assert(pthread_mutex_init(&pool->release_mutex, NULL) == 0);
    pool->has_directive          = true;
    pool->n_slots                = slot_count;
    pool->bind_generation        = 7;
    pool->release_slot_count     = slot_count;
    pool->next_acquire_serial    = 1;
    pool->ops                    = &TEST_OPS;
    pool->latest_acquire_sync_fd = -1;
}

static void destroy_pool_state(ww_pool_t* pool) {
    if (pool->latest_acquire_sync_fd >= 0) close(pool->latest_acquire_sync_fd);
    assert(pthread_mutex_destroy(&pool->release_mutex) == 0);
}

static ww_pool_slot_identity_t identity(uint64_t previous_release_point, uint64_t serial) {
    return (ww_pool_slot_identity_t) {
        .bind_generation        = 7,
        .slot_index             = 0,
        .previous_release_point = previous_release_point,
        .acquire_serial         = serial,
    };
}

static void test_abort_requires_exact_identity(void) {
    ww_pool_t pool;
    init_pool(&pool, 1);
    ww_pool_slot_identity_t acquired = identity(12, 3);
    pool.pending_acquire             = (ww_pool_pending_acquire_t) {
        .identity = acquired,
        .active   = true,
    };

    ww_pool_slot_identity_t stale = identity(12, 4);
    assert(ww_bridge_pool_abort_acquired_slot(&pool, &stale) == -ESTALE);
    assert(pool.pending_acquire.active);
    assert(ww_bridge_pool_abort_acquired_slot(&pool, &acquired) == 0);
    assert(! pool.pending_acquire.active);

    destroy_pool_state(&pool);
}

static void test_send_failure_does_not_commit_release_point(void) {
    ww_pool_t pool;
    init_pool(&pool, 1);
    pool.release_point               = 20;
    pool.last_release_point[0]       = 19;
    ww_pool_slot_identity_t acquired = identity(19, 5);
    pool.pending_acquire             = (ww_pool_pending_acquire_t) {
        .identity = acquired,
        .active   = true,
    };

    ww_pool_slot_submit_result_t result;
    assert(ww_bridge_pool_submit_acquired_slot(&pool, -1, &acquired, -1, &result) == 0);
    assert(result.status == WW_POOL_SLOT_SUBMIT_SESSION_LOST);
    assert(result.release_point == 21);
    assert(result.error_code != 0);
    assert(pool.session_lost);
    assert(pool.release_point == 20);
    assert(pool.last_release_point[0] == 19);
    assert(! pool.pending_submit.active);
    assert(! pool.pending_acquire.active);

    destroy_pool_state(&pool);
}

static void test_acquire_any_scans_fresh_slots(void) {
    const uint32_t slot_counts[] = { 1, 2, 3, WW_POOL_MAX_SLOT_COUNT };
    for (size_t count_index = 0; count_index < sizeof(slot_counts) / sizeof(slot_counts[0]);
         ++count_index) {
        ww_pool_t pool;
        init_pool(&pool, slot_counts[count_index]);
        for (uint32_t expected = 0; expected < slot_counts[count_index]; ++expected) {
            ww_pool_slot_acquire_result_t result;
            assert(ww_bridge_pool_try_acquire_any_for_render(&pool, &result) == 0);
            assert(result.status == WW_POOL_SLOT_ACQUIRE_READY_UNUSED);
            assert(result.identity.slot_index == expected);
            assert(result.slot.index == expected);
            assert(result.slot.gl_export_fbo == expected + 100);
            assert(ww_bridge_pool_abort_acquired_slot(&pool, &result.identity) == 0);
        }
        destroy_pool_state(&pool);
    }
}

static void test_acquire_any_skips_submitted_slot_for_fresh_slot(void) {
    ww_pool_t pool;
    init_pool(&pool, 3);
    pool.last_release_point[0] = 9;

    ww_pool_slot_acquire_result_t result;
    assert(ww_bridge_pool_try_acquire_any_for_render(&pool, &result) == 0);
    assert(result.status == WW_POOL_SLOT_ACQUIRE_READY_UNUSED);
    assert(result.identity.slot_index == 1);
    assert(ww_bridge_pool_abort_acquired_slot(&pool, &result.identity) == 0);

    destroy_pool_state(&pool);
}

static waywallen_frame_t recv_frame_ready(int sock) {
    uint16_t opcode   = 0;
    uint8_t* body     = NULL;
    size_t   body_len = 0;
    int      fds[2]   = { -1, -1 };
    size_t   n_fds    = 0;
    assert(ww_bridge_recv_frame(sock, &opcode, &body, &body_len, fds, 2, &n_fds) == 0);
    assert(opcode == WW_EVT_FRAME_READY);
    assert(n_fds == 1);
    ww_evt_frame_ready_t event;
    assert(ww_evt_frame_ready_decode(body, body_len, &event) == 0);
    waywallen_frame_t frame = event.frame;
    ww_evt_frame_ready_free(&event);
    free(body);
    close(fds[0]);
    return frame;
}

static void test_republish_requires_content(void) {
    ww_pool_t pool;
    init_pool(&pool, 1);
    ww_pool_republish_result_t result;
    assert(ww_bridge_pool_try_republish_latest(&pool, -1, &result) == 0);
    assert(result.status == WW_POOL_REPUBLISH_NO_CONTENT);
    assert(pool.release_point == 0);
    destroy_pool_state(&pool);
}

static void test_republish_reuses_latest_slot_with_new_release_point(void) {
    int sockets[2];
    int fence[2];
    assert(socketpair(AF_UNIX, SOCK_STREAM, 0, sockets) == 0);
    assert(pipe(fence) == 0);

    ww_pool_t pool;
    init_pool(&pool, 2);
    ww_pool_slot_acquire_result_t acquired;
    assert(ww_bridge_pool_try_acquire_any_for_render(&pool, &acquired) == 0);
    assert(acquired.status == WW_POOL_SLOT_ACQUIRE_READY_UNUSED);
    assert(acquired.identity.slot_index == 0);

    ww_pool_slot_submit_result_t submitted;
    assert(ww_bridge_pool_submit_acquired_slot(
               &pool, sockets[0], &acquired.identity, fence[0], &submitted) == 0);
    assert(submitted.status == WW_POOL_SLOT_SUBMIT_SUBMITTED);
    close(fence[1]);
    waywallen_frame_t first = recv_frame_ready(sockets[1]);
    assert(first.image_index == 0);
    assert(first.sequence == 1);
    assert(pool.latest_content_valid);
    assert(pool.latest_slot_index == 0);

    /* Unit-test state has no kernel syncobj. Mark the slot released so
     * the republish path can exercise its transaction and wire event. */
    pool.last_release_point[0] = 0;
    ww_pool_republish_result_t republished;
    assert(ww_bridge_pool_try_republish_latest(&pool, sockets[0], &republished) == 0);
    assert(republished.status == WW_POOL_REPUBLISH_PUBLISHED);
    assert(republished.slot_index == 0);
    assert(republished.sequence == 2);
    assert(republished.release_point == 2);
    waywallen_frame_t second = recv_frame_ready(sockets[1]);
    assert(second.image_index == 0);
    assert(second.sequence == 2);
    assert(second.release_point == 2);

    pool.last_release_point[0] = 0;
    pool.next_slot             = 0;
    ww_pool_slot_acquire_result_t overwrite;
    assert(ww_bridge_pool_try_acquire_any_for_render(&pool, &overwrite) == 0);
    assert(overwrite.status == WW_POOL_SLOT_ACQUIRE_READY_UNUSED);
    assert(! pool.latest_content_valid);
    assert(ww_bridge_pool_abort_acquired_slot(&pool, &overwrite.identity) == 0);
    assert(ww_bridge_pool_try_republish_latest(&pool, sockets[0], &republished) == 0);
    assert(republished.status == WW_POOL_REPUBLISH_NO_CONTENT);

    close(sockets[0]);
    close(sockets[1]);
    destroy_pool_state(&pool);
}

static unsigned allocated_slots;
static unsigned fail_slot;

static int fork_allocation(const ww_pool_t* source, ww_pool_t* candidate) {
    (void)source;
    candidate->backend_data = calloc(WW_POOL_MAX_SLOTS, sizeof(bool));
    return candidate->backend_data ? 0 : -ENOMEM;
}

static int alloc_slot(ww_pool_t* pool, uint32_t index, ww_pool_slot_layout_t* layout) {
    if (index == fail_slot) return -ENOMEM;
    bool* live  = pool->backend_data;
    live[index] = true;
    ++allocated_slots;
    layout->plane_count = 1;
    layout->fds[0]      = open("/dev/null", O_RDONLY);
    assert(layout->fds[0] >= 0);
    layout->strides[0] = pool->cur.extent.width * 4;
    layout->sizes[0]   = (uint64_t)layout->strides[0] * pool->cur.extent.height;
    return 0;
}

static void free_slot(ww_pool_t* pool, uint32_t index) {
    bool* live = pool->backend_data;
    if (live[index]) {
        live[index] = false;
        --allocated_slots;
    }
}

static void free_backend(ww_pool_t* pool) { free(pool->backend_data); }

static const struct ww_pool_backend_ops ALLOCATION_OPS = {
    .fork_allocation    = fork_allocation,
    .alloc_slot         = alloc_slot,
    .free_slot          = free_slot,
    .destroy            = free_backend,
    .populate_slot_view = populate_slot_view,
};

static void test_render_target_candidate_rollback_and_commit(void) {
    ww_pool_t pool;
    init_pool(&pool, 0);
    pool.ops               = &ALLOCATION_OPS;
    pool.backend_data      = calloc(WW_POOL_MAX_SLOTS, sizeof(bool));
    pool.caps_advertised   = true;
    pool.caps.extent_max_w = pool.caps.extent_max_h = 4096;
    pool.probe_width                                = 1920;
    pool.probe_height                               = 1080;
    pool.cur.extent                                 = (waywallen_extent_t) { 1920, 1080 };
    pool.release_point                              = 27;
    for (unsigned i = 0; i < WW_POOL_MAX_SLOTS; ++i)
        for (unsigned j = 0; j < WW_POOL_MAX_PLANES; ++j) pool.slots[i].fds[j] = -1;
    fail_slot = WW_POOL_MAX_SLOTS;
    assert(alloc_slot(&pool, 0, &pool.slots[0]) == 0);
    pool.n_slots                  = 1;
    int                 old_fd    = pool.slots[0].fds[0];
    ww_pool_directive_t directive = {
        .count         = 3,
        .sync_mode     = WW_SYNC_SYNCOBJ_TIMELINE,
        .path          = WAYWALLEN_BUFFER_PATH_COMPAT_LINEAR,
        .memory_source = WAYWALLEN_BUFFER_MEMORY_SOURCE_GPU_LINEAR,
        .format        = { .fourcc = 1, .plane_count = 1 },
    };
    fail_slot = 1;
    assert(ww_bridge_pool_prepare_target(&pool, &directive, 1280, 720) == -ENOMEM);
    assert(! pool.candidate && allocated_slots == 1);
    assert(pool.n_slots == 1 && pool.bind_generation == 7);
    assert(pool.cur.extent.width == 1920 && fcntl(old_fd, F_GETFD) >= 0);
    fail_slot = WW_POOL_MAX_SLOTS;
    assert(ww_bridge_pool_prepare_target(&pool, &directive, 1280, 720) == 0);
    assert(ww_bridge_pool_prepare_target(&pool, &directive, 640, 360) == -EBUSY);
    assert(allocated_slots == 4);
    ww_pool_slot_t slot;
    assert(ww_bridge_pool_candidate_slot(&pool, 0, &slot) == 0);
    assert(slot.width == 1280 && slot.height == 720);
    ww_bridge_pool_abort_target(&pool);
    assert(allocated_slots == 1 && pool.cur.extent.width == 1920);
    assert(ww_bridge_pool_prepare_target(&pool, &directive, 1280, 720) == 0);
    int sockets[2];
    assert(socketpair(AF_UNIX, SOCK_STREAM, 0, sockets) == 0);
    uint64_t generation;
    assert(ww_bridge_pool_commit_target(&pool, sockets[0], &generation) == 0);
    assert(generation == 8 && pool.release_point == 27);
    assert(! pool.candidate && allocated_slots == 3 && pool.cur.extent.width == 1280);
    assert(fcntl(old_fd, F_GETFD) == -1);
    for (unsigned i = 0; i < pool.n_slots; ++i) {
        free_slot(&pool, i);
        close(pool.slots[i].fds[0]);
    }
    free_backend(&pool);
    close(sockets[0]);
    close(sockets[1]);
    destroy_pool_state(&pool);
    assert(allocated_slots == 0);
}

int main(void) {
    test_render_target_candidate_rollback_and_commit();
    test_abort_requires_exact_identity();
    test_send_failure_does_not_commit_release_point();
    test_acquire_any_scans_fresh_slots();
    test_acquire_any_skips_submitted_slot_for_fresh_slot();
    test_republish_requires_content();
    test_republish_reuses_latest_slot_with_new_release_point();
    return 0;
}
