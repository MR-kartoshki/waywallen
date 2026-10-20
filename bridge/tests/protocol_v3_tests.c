#include <waywallen-bridge/bridge.h>
#include <waywallen-bridge/resolution.h>

#include <assert.h>
#include <math.h>
#include <stdlib.h>
#include <string.h>

static void test_subscription_codec(void) {
    char* kinds[] = { "audio", "pointer" };
    ww_evt_set_event_subscriptions_t input   = {
        .subscription = {
            .revision = 7,
            .kinds    = { .count = 2, .data = kinds },
        },
    };
    ww_buf_t encoded;
    ww_buf_init(&encoded);
    assert(ww_evt_set_event_subscriptions_encode(&input, &encoded) == 0);

    ww_evt_set_event_subscriptions_t decoded;
    assert(ww_evt_set_event_subscriptions_decode(encoded.data, encoded.len, &decoded) == 0);
    assert(decoded.subscription.revision == 7);
    assert(decoded.subscription.kinds.count == 2);
    assert(strcmp(decoded.subscription.kinds.data[0], "audio") == 0);
    assert(strcmp(decoded.subscription.kinds.data[1], "pointer") == 0);
    ww_evt_set_event_subscriptions_free(&decoded);
    ww_buf_free(&encoded);
}

static void test_request_frame_codec(void) {
    ww_evt_in_request_frame_t input = { 0 };
    ww_buf_t                  encoded;
    ww_buf_init(&encoded);
    assert(ww_evt_in_request_frame_encode(&input, &encoded) == 0);

    ww_evt_in_request_frame_t decoded;
    assert(ww_evt_in_request_frame_decode(encoded.data, encoded.len, &decoded) == 0);
    assert(ww_evt_in_request_frame_expected_fds(&decoded) == 0);
    ww_evt_in_request_frame_free(&decoded);
    const uint8_t trailing = 0;
    assert(ww_evt_in_request_frame_decode(&trailing, 1, &decoded) == WW_ERR_TRAILING);
    ww_buf_free(&encoded);
}

static void test_set_log_level_codec(void) {
    ww_evt_in_set_log_level_t input = { .level = WAYWALLEN_LOG_LEVEL_DEBUG };
    ww_buf_t                  encoded;
    ww_buf_init(&encoded);
    assert(ww_evt_in_set_log_level_encode(&input, &encoded) == 0);

    ww_evt_in_set_log_level_t decoded;
    assert(ww_evt_in_set_log_level_decode(encoded.data, encoded.len, &decoded) == 0);
    assert(decoded.level == WAYWALLEN_LOG_LEVEL_DEBUG);
    ww_evt_in_set_log_level_free(&decoded);
    ww_buf_free(&encoded);
}

static void test_subscription_ack_view(void) {
    ww_bridge_control_t control                   = { .op = WW_EVT_IN_EVENT_SUBSCRIPTIONS_APPLIED };
    waywallen_event_subscription_result_t* result = &control.u.event_subscriptions_applied.result;
    result->revision                              = 9;
    result->status                                = WAYWALLEN_EVENT_SUBSCRIPTION_STATUS_APPLIED;
    result->kinds.count                           = 1;
    result->kinds.data                            = calloc(1, sizeof(char*));
    result->kinds.data[0]                         = strdup("audio");
    result->reason                                = strdup("");

    assert(result->revision == 9);
    assert(result->kinds.count == 1);
    assert(strcmp(result->kinds.data[0], "audio") == 0);
    ww_bridge_control_free(&control);
}

static void test_audio_helper_validates_complete_windows_and_end(void) {
    ww_bridge_control_t       control = { .op = WW_EVT_IN_AUDIO_WINDOW };
    waywallen_audio_window_t* window  = &control.u.audio_window.window;
    window->subscription_revision     = 4;
    window->generation                = 5;
    window->sequence                  = 6;
    window->captured_at_ns            = 7;
    window->end_sample_frame          = 4096;
    window->format.sample_rate_hz     = WW_BRIDGE_AUDIO_SAMPLE_RATE;
    window->format.channels           = WW_BRIDGE_AUDIO_CHANNELS;
    window->frames                    = WW_BRIDGE_AUDIO_WINDOW_FRAMES;
    window->samples.count             = WW_BRIDGE_AUDIO_SAMPLE_COUNT;
    window->samples.data              = calloc(WW_BRIDGE_AUDIO_SAMPLE_COUNT, sizeof(float));
    for (uint32_t index = 0; index < WW_BRIDGE_AUDIO_SAMPLE_COUNT; ++index) {
        window->samples.data[index] = (float)index / WW_BRIDGE_AUDIO_SAMPLE_COUNT;
    }

    ww_bridge_audio_window_t audio;
    assert(ww_bridge_audio_window_from_control(&control, &audio) == 0);
    assert(audio.subscription_revision == 4);
    assert(audio.frames == WW_BRIDGE_AUDIO_WINDOW_FRAMES);
    assert(audio.samples[1] > 0.0f);

    window->samples.data[0] = NAN;
    assert(ww_bridge_audio_window_from_control(&control, &audio) != 0);
    window->samples.data[0] = 0.0f;
    window->samples.count -= 1;
    assert(ww_bridge_audio_window_from_control(&control, &audio) != 0);
    window->samples.count = 0;
    free(window->samples.data);
    window->samples.data          = NULL;
    window->frames                = 0;
    window->format.sample_rate_hz = 0;
    window->format.channels       = 0;
    window->flags                 = WW_BRIDGE_AUDIO_END_OF_STREAM;
    assert(ww_bridge_audio_window_from_control(&control, &audio) == 0);
    assert(audio.flags == WW_BRIDGE_AUDIO_END_OF_STREAM);
    ww_bridge_control_free(&control);
}

/* An untrusted count larger than the remaining input must not force a huge allocation. */
static void test_decoder_rejects_oversized_array_count(void) {
    /* setting_changed body is a bare kv_list: [u32 count]. */
    const uint8_t               kv_huge[4] = { 0xff, 0xff, 0xff, 0xff };
    ww_evt_in_setting_changed_t settings;
    assert(ww_evt_in_setting_changed_decode(kv_huge, sizeof(kv_huge), &settings) ==
           WW_ERR_BAD_ARRAY);

    /* set_event_subscriptions body: [u64 revision][u32 kinds count]. */
    const uint8_t subs_huge[12] = {
        0,    0,    0,    0,    0, 0, 0, 0, /* revision = 0 */
        0xff, 0xff, 0xff, 0xff,             /* kinds.count = UINT32_MAX */
    };
    ww_evt_set_event_subscriptions_t subs;
    assert(ww_evt_set_event_subscriptions_decode(subs_huge, sizeof(subs_huge), &subs) ==
           WW_ERR_BAD_ARRAY);
}

static void test_render_target_tagged_codec(void) {
    assert(WW_RESOLUTION_AUTO == -2);
    assert(ww_resolution_sanitize(-2) == WW_RESOLUTION_AUTO);
    assert(ww_resolution_sanitize(-1) == WW_RESOLUTION_CUSTOM);
    assert(ww_resolution_sanitize(-3) == WW_RESOLUTION_1080P);
    assert(ww_resolution_sanitize(5) == WW_RESOLUTION_1080P);
    assert(ww_resolution_sanitize(6) == WW_RESOLUTION_1080P);
    assert(WW_RESOLUTION_ORIGIN == 0 && WW_RESOLUTION_2160P == 4);
    uint32_t width = 1920, height = 1080;
    ww_resolution_apply_short_edge(540, WW_RESOLUTION_CAP_DEFAULT, &width, &height);
    assert(width == 960 && height == 540);
    ww_resolution_apply_short_edge(8192, WW_RESOLUTION_CAP_DEFAULT, &width, &height);
    assert(width == 960 && height == 540);
    width  = 1080;
    height = 1920;
    ww_resolution_apply_short_edge(540, WW_RESOLUTION_CAP_DEFAULT, &width, &height);
    assert(width == 540 && height == 960);
    ww_evt_in_configure_render_target_t
             input = { .config = {
                           .revision              = 17,
                           .capabilities_revision = 9,
                           .width                 = 1920,
                           .height                = 1080,
                           .fourcc                = 42,
                           .modifier              = 7,
                           .plane_count           = 1,
                           .sync_mode             = 4,
                           .color                 = 1,
                           .mem_hint              = 1,
                           .count                 = 3,
                           .path                  = WAYWALLEN_BUFFER_PATH_OPTIMIZED_SAME_DEVICE,
                           .memory_source         = WAYWALLEN_BUFFER_MEMORY_SOURCE_GPU_NATIVE,
                       } };
    ww_buf_t encoded;
    ww_buf_init(&encoded);
    assert(ww_evt_in_configure_render_target_encode(&input, &encoded) == 0);
    ww_evt_in_configure_render_target_t decoded;
    assert(ww_evt_in_configure_render_target_decode(encoded.data, encoded.len, &decoded) == 0);
    assert(decoded.config.revision == 17 && decoded.config.width == 1920);
    assert(decoded.config.modifier == 7);
    ww_evt_in_configure_render_target_free(&decoded);
    const uint8_t empty[] = { 1, 0, 0, 0, 0, 0, 0, 0 };
    assert(ww_evt_in_configure_render_target_decode(empty, sizeof(empty), &decoded) ==
           WW_ERR_SHORT);
    assert(ww_evt_in_configure_render_target_decode(encoded.data, encoded.len - 1, &decoded) != 0);
    /* Unknown top-level fields must not invalidate known required fields. */
    uint8_t* extended = calloc(encoded.len + 16, 1);
    memcpy(extended, encoded.data, encoded.len);
    uint32_t body_size = (uint32_t)encoded.len - 8 + 16;
    for (unsigned i = 0; i < 4; ++i) extended[4 + i] = (uint8_t)(body_size >> (i * 8));
    extended[encoded.len]     = 99;
    extended[encoded.len + 4] = 1;
    extended[encoded.len + 8] = 4;
    assert(ww_evt_in_configure_render_target_decode(extended, encoded.len + 16, &decoded) == 0);
    assert(decoded.config.height == 1080);
    ww_evt_in_configure_render_target_free(&decoded);
    free(extended);
    ww_buf_free(&encoded);
}

static void test_render_target_limits(void) {
    waywallen_render_target_capabilities_t caps = {
        .revision         = 1,
        .mode             = 1,
        .reference_width  = 3840,
        .reference_height = 2160,
        .min_width        = 2,
        .min_height       = 2,
        .max_width        = 3840,
        .max_height       = 2160,
        .width_alignment  = 2,
        .height_alignment = 2,
    };
    waywallen_render_target_config_t config = {
        .revision              = 1,
        .capabilities_revision = 1,
        .width                 = 1920,
        .height                = 1080,
    };
    waywallen_buffer_directive_t directive;
    assert(ww_bridge_render_target_directive(&caps, &config, &directive) == 0);
    config.width = 1900;
    assert(ww_bridge_render_target_directive(&caps, &config, &directive) != 0);
    config.width = 1921;
    assert(ww_bridge_render_target_directive(&caps, &config, &directive) != 0);
    config.width                 = 1920;
    config.capabilities_revision = 2;
    assert(ww_bridge_render_target_directive(&caps, &config, &directive) != 0);
    config.capabilities_revision = 1;
    caps.mode                    = 2;
    caps.max_width = caps.max_height = 65535;
    config.width                     = 7680;
    config.height                    = 4320;
    assert(ww_bridge_render_target_directive(&caps, &config, &directive) == 0);
    config.width  = 15360;
    config.height = 8640;
    assert(ww_bridge_render_target_directive(&caps, &config, &directive) != 0);
    caps.reference_width  = 2160;
    caps.reference_height = 3840;
    config.width          = 4320;
    config.height         = 7680;
    assert(ww_bridge_render_target_directive(&caps, &config, &directive) == 0);
    caps.reference_width = caps.reference_height = 1000;
    config.width = config.height = 7680;
    assert(ww_bridge_render_target_directive(&caps, &config, &directive) != 0);
}

int main(void) {
    test_render_target_limits();
    test_render_target_tagged_codec();
    test_subscription_codec();
    test_request_frame_codec();
    test_set_log_level_codec();
    test_subscription_ack_view();
    test_audio_helper_validates_complete_windows_and_end();
    test_decoder_rejects_oversized_array_count();
    return 0;
}
