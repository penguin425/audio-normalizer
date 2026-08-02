#include <gst/audio/audio.h>
#include <gst/audio/gstaudiofilter.h>
#include <gst/base/gstbasetransform.h>
#include <gst/gst.h>

#include "forge_normalizer.h"

#ifndef FORGE_PLUGIN_VERSION
#define FORGE_PLUGIN_VERSION "0.119.0"
#endif

typedef struct _GstForgeNormalizer GstForgeNormalizer;
typedef struct _GstForgeNormalizerClass GstForgeNormalizerClass;

struct _GstForgeNormalizer {
    GstAudioFilter parent;
    ForgeLiveV1 *live;
    guint sample_rate_hz;
    guint channels;
    gdouble gain_db;
    gdouble ceiling_dbtp;
    gdouble attack_ms;
    gdouble release_ms;
    gboolean flushed;
    GstClockTime last_end;
};

struct _GstForgeNormalizerClass {
    GstAudioFilterClass parent_class;
};

#define GST_TYPE_FORGE_NORMALIZER (gst_forge_normalizer_get_type())
#define GST_FORGE_NORMALIZER(obj) \
    (G_TYPE_CHECK_INSTANCE_CAST((obj), GST_TYPE_FORGE_NORMALIZER, GstForgeNormalizer))
GType gst_forge_normalizer_get_type(void);

G_DEFINE_TYPE(GstForgeNormalizer, gst_forge_normalizer, GST_TYPE_AUDIO_FILTER)

enum {
    PROP_0,
    PROP_GAIN_DB,
    PROP_CEILING_DBTP,
    PROP_ATTACK_MS,
    PROP_RELEASE_MS,
    N_PROPERTIES,
};

static GParamSpec *properties[N_PROPERTIES];

static void gst_forge_clear_error(char *error, gsize capacity) {
    if (error != NULL && capacity > 0u) {
        error[0] = '\0';
    }
}

static void gst_forge_report_error(GstForgeNormalizer *self, const char *operation, const char *error) {
    GST_ELEMENT_ERROR(
        GST_ELEMENT(self),
        STREAM,
        FAILED,
        ("Forge %s failed", operation),
        ("%s", error != NULL && error[0] != '\0' ? error : "unknown Forge error"));
}

static gboolean gst_forge_create_live(GstForgeNormalizer *self) {
    char error[256];
    gst_forge_clear_error(error, sizeof(error));
    if (self->sample_rate_hz == 0u || self->channels == 0u) {
        return TRUE;
    }
    ForgeLiveConfigV1 config = {0};
    config.struct_size = (guint32)sizeof(config);
    config.api_version = forge_normalizer_c_api_version();
    config.sample_rate_hz = self->sample_rate_hz;
    config.channels = self->channels;
    config.initial_gain_db = self->gain_db;
    config.ceiling_dbtp = self->ceiling_dbtp;
    config.attack_ms = self->attack_ms;
    config.release_ms = self->release_ms;
    self->live = forge_normalizer_live_create_v1(&config, error, sizeof(error));
    if (self->live == NULL) {
        gst_forge_report_error(self, "processor creation", error);
        return FALSE;
    }
    self->flushed = FALSE;
    self->last_end = GST_CLOCK_TIME_NONE;
    return TRUE;
}

static void gst_forge_destroy_live(GstForgeNormalizer *self) {
    if (self->live != NULL) {
        forge_normalizer_live_destroy_v1(self->live);
        self->live = NULL;
    }
    self->flushed = FALSE;
    self->last_end = GST_CLOCK_TIME_NONE;
}

static gboolean gst_forge_reset_live(GstForgeNormalizer *self) {
    gst_forge_destroy_live(self);
    return gst_forge_create_live(self);
}

static gboolean gst_forge_setup(GstAudioFilter *filter, const GstAudioInfo *info) {
    GstForgeNormalizer *self = GST_FORGE_NORMALIZER(filter);
    gst_forge_destroy_live(self);
    if (GST_AUDIO_INFO_FORMAT(info) != GST_AUDIO_FORMAT_F32LE ||
        GST_AUDIO_INFO_LAYOUT(info) != GST_AUDIO_LAYOUT_INTERLEAVED) {
        GST_ELEMENT_ERROR(
            GST_ELEMENT(self),
            STREAM,
            FORMAT,
            ("forge_normalizer requires interleaved F32LE audio"),
            ("received format %s with %s layout",
             gst_audio_format_to_string(GST_AUDIO_INFO_FORMAT(info)),
             GST_AUDIO_INFO_LAYOUT(info) == GST_AUDIO_LAYOUT_NON_INTERLEAVED
                 ? "non-interleaved"
                 : "unsupported"));
        return FALSE;
    }
    self->sample_rate_hz = GST_AUDIO_INFO_RATE(info);
    self->channels = GST_AUDIO_INFO_CHANNELS(info);
    if (self->sample_rate_hz < 8000u || self->sample_rate_hz > 384000u ||
        self->channels == 0u || self->channels > 64u) {
        GST_ELEMENT_ERROR(
            GST_ELEMENT(self),
            STREAM,
            FORMAT,
            ("forge_normalizer received audio outside the v1 C ABI limits"),
            ("rate=%u channels=%u", self->sample_rate_hz, self->channels));
        return FALSE;
    }
    return gst_forge_create_live(self);
}

static GstFlowReturn gst_forge_transform_ip(GstBaseTransform *transform, GstBuffer *buffer) {
    GstForgeNormalizer *self = GST_FORGE_NORMALIZER(transform);
    if (self->live == NULL || self->flushed) {
        gst_forge_report_error(self, "processing", "processor is not active");
        return GST_FLOW_ERROR;
    }
    gsize bytes_per_frame = (gsize)self->channels * sizeof(float);
    gsize size = gst_buffer_get_size(buffer);
    if (bytes_per_frame == 0u || size % bytes_per_frame != 0u) {
        gst_forge_report_error(self, "processing", "buffer is not frame aligned");
        return GST_FLOW_ERROR;
    }
    GstMapInfo map = GST_MAP_INFO_INIT;
    if (!gst_buffer_map(buffer, &map, GST_MAP_READWRITE)) {
        gst_forge_report_error(self, "processing", "buffer is not writable");
        return GST_FLOW_ERROR;
    }
    gsize frames = size / bytes_per_frame;
    char error[256];
    ForgeStatus status = forge_normalizer_live_process_interleaved_f32_v1(
        self->live,
        (float *)map.data,
        frames,
        error,
        sizeof(error));
    gst_buffer_unmap(buffer, &map);
    if (status != FORGE_STATUS_OK) {
        gst_forge_report_error(self, "processing", error);
        return GST_FLOW_ERROR;
    }

    GstClockTime pts = GST_BUFFER_PTS(buffer);
    GstClockTime duration = GST_BUFFER_DURATION(buffer);
    if (GST_CLOCK_TIME_IS_VALID(pts)) {
        if (GST_CLOCK_TIME_IS_VALID(duration)) {
            self->last_end = pts + duration;
        } else {
            self->last_end = pts +
                gst_util_uint64_scale(frames, GST_SECOND, self->sample_rate_hz);
        }
    } else if (GST_CLOCK_TIME_IS_VALID(self->last_end)) {
        self->last_end +=
            gst_util_uint64_scale(frames, GST_SECOND, self->sample_rate_hz);
    }
    return GST_FLOW_OK;
}

static gboolean gst_forge_flush_tail(GstForgeNormalizer *self, GstBaseTransform *transform) {
    if (self->live == NULL || self->flushed) {
        return TRUE;
    }
    size_t latency = forge_normalizer_live_latency_frames_v1(self->live);
    gsize bytes_per_frame = (gsize)self->channels * sizeof(float);
    if (latency == 0u || bytes_per_frame == 0u) {
        self->flushed = TRUE;
        return TRUE;
    }
    if (latency > G_MAXSIZE / bytes_per_frame) {
        gst_forge_report_error(self, "flush", "tail buffer is too large");
        return FALSE;
    }
    GstBuffer *tail = gst_buffer_new_allocate(NULL, latency * bytes_per_frame, NULL);
    if (tail == NULL) {
        gst_forge_report_error(self, "flush", "unable to allocate tail buffer");
        return FALSE;
    }
    GstMapInfo map = GST_MAP_INFO_INIT;
    if (!gst_buffer_map(tail, &map, GST_MAP_WRITE)) {
        gst_buffer_unref(tail);
        gst_forge_report_error(self, "flush", "tail buffer is not writable");
        return FALSE;
    }
    size_t written = 0u;
    char error[256];
    ForgeStatus status = forge_normalizer_live_flush_interleaved_f32_v1(
        self->live,
        (float *)map.data,
        latency,
        &written,
        error,
        sizeof(error));
    gst_buffer_unmap(tail, &map);
    if (status != FORGE_STATUS_OK) {
        gst_buffer_unref(tail);
        gst_forge_report_error(self, "flush", error);
        return FALSE;
    }
    gst_buffer_set_size(tail, written * bytes_per_frame);
    if (GST_CLOCK_TIME_IS_VALID(self->last_end)) {
        GST_BUFFER_PTS(tail) = self->last_end;
    }
    GST_BUFFER_DURATION(tail) =
        gst_util_uint64_scale(written, GST_SECOND, self->sample_rate_hz);
    GstFlowReturn flow = gst_pad_push(GST_BASE_TRANSFORM_SRC_PAD(transform), tail);
    self->flushed = TRUE;
    if (flow != GST_FLOW_OK) {
        GST_ELEMENT_ERROR(
            GST_ELEMENT(self),
            STREAM,
            FAILED,
            ("Forge tail buffer could not be pushed"),
            ("GStreamer returned flow %s", gst_flow_get_name(flow)));
        return FALSE;
    }
    return TRUE;
}

static gboolean gst_forge_sink_event(GstBaseTransform *transform, GstEvent *event) {
    GstForgeNormalizer *self = GST_FORGE_NORMALIZER(transform);
    GstEventType type = GST_EVENT_TYPE(event);
    if (type == GST_EVENT_EOS && !gst_forge_flush_tail(self, transform)) {
        gst_event_unref(event);
        return FALSE;
    }
    if (type == GST_EVENT_FLUSH_STOP && !gst_forge_reset_live(self)) {
        gst_event_unref(event);
        return FALSE;
    }
    GstBaseTransformClass *parent = GST_BASE_TRANSFORM_CLASS(gst_forge_normalizer_parent_class);
    return parent->sink_event(transform, event);
}

static gboolean gst_forge_query(GstBaseTransform *transform, GstPadDirection direction, GstQuery *query) {
    GstForgeNormalizer *self = GST_FORGE_NORMALIZER(transform);
    GstBaseTransformClass *parent = GST_BASE_TRANSFORM_CLASS(gst_forge_normalizer_parent_class);
    if (GST_QUERY_TYPE(query) != GST_QUERY_LATENCY) {
        return parent->query(transform, direction, query);
    }
    gboolean result = parent->query(transform, direction, query);
    GstClockTime own = 0;
    if (self->live != NULL && self->sample_rate_hz != 0u) {
        own = gst_util_uint64_scale(
            forge_normalizer_live_latency_frames_v1(self->live), GST_SECOND,
            self->sample_rate_hz);
    }
    if (!result) {
        gst_query_set_latency(query, TRUE, own, own);
        return TRUE;
    }
    gboolean live;
    GstClockTime min_latency;
    GstClockTime max_latency;
    gst_query_parse_latency(query, &live, &min_latency, &max_latency);
    if (GST_CLOCK_TIME_IS_VALID(min_latency)) {
        min_latency += own;
    }
    if (GST_CLOCK_TIME_IS_VALID(max_latency)) {
        max_latency += own;
    }
    gst_query_set_latency(query, live, min_latency, max_latency);
    return TRUE;
}

static void gst_forge_set_property(GObject *object, guint property_id, const GValue *value, GParamSpec *pspec) {
    GstForgeNormalizer *self = GST_FORGE_NORMALIZER(object);
    char error[256];
    switch (property_id) {
    case PROP_GAIN_DB:
        self->gain_db = g_value_get_double(value);
        if (self->live != NULL) {
            ForgeStatus status = forge_normalizer_live_set_target_gain_db_v1(
                self->live, self->gain_db, error, sizeof(error));
            if (status != FORGE_STATUS_OK) {
                GST_WARNING_OBJECT(self, "gain update failed: %s", error);
            }
        }
        break;
    case PROP_CEILING_DBTP:
        self->ceiling_dbtp = g_value_get_double(value);
        if (self->live != NULL) {
            ForgeStatus status = forge_normalizer_live_set_ceiling_dbtp_v1(
                self->live, self->ceiling_dbtp, error, sizeof(error));
            if (status != FORGE_STATUS_OK) {
                GST_WARNING_OBJECT(self, "ceiling update failed: %s", error);
            }
        }
        break;
    case PROP_ATTACK_MS:
        self->attack_ms = g_value_get_double(value);
        break;
    case PROP_RELEASE_MS:
        self->release_ms = g_value_get_double(value);
        break;
    default:
        G_OBJECT_WARN_INVALID_PROPERTY_ID(object, property_id, pspec);
        break;
    }
}

static void gst_forge_get_property(GObject *object, guint property_id, GValue *value, GParamSpec *pspec) {
    GstForgeNormalizer *self = GST_FORGE_NORMALIZER(object);
    switch (property_id) {
    case PROP_GAIN_DB:
        g_value_set_double(value, self->gain_db);
        break;
    case PROP_CEILING_DBTP:
        g_value_set_double(value, self->ceiling_dbtp);
        break;
    case PROP_ATTACK_MS:
        g_value_set_double(value, self->attack_ms);
        break;
    case PROP_RELEASE_MS:
        g_value_set_double(value, self->release_ms);
        break;
    default:
        G_OBJECT_WARN_INVALID_PROPERTY_ID(object, property_id, pspec);
        break;
    }
}

static gboolean gst_forge_start(GstBaseTransform *transform) {
    GstForgeNormalizer *self = GST_FORGE_NORMALIZER(transform);
    self->flushed = FALSE;
    self->last_end = GST_CLOCK_TIME_NONE;
    return TRUE;
}

static gboolean gst_forge_stop(GstBaseTransform *transform) {
    GstForgeNormalizer *self = GST_FORGE_NORMALIZER(transform);
    gst_forge_destroy_live(self);
    return TRUE;
}

static void gst_forge_finalize(GObject *object) {
    GstForgeNormalizer *self = GST_FORGE_NORMALIZER(object);
    gst_forge_destroy_live(self);
    G_OBJECT_CLASS(gst_forge_normalizer_parent_class)->finalize(object);
}

static void gst_forge_normalizer_class_init(GstForgeNormalizerClass *klass) {
    GObjectClass *object_class = G_OBJECT_CLASS(klass);
    GstElementClass *element_class = GST_ELEMENT_CLASS(klass);
    GstAudioFilterClass *audio_class = GST_AUDIO_FILTER_CLASS(klass);
    GstBaseTransformClass *transform_class = GST_BASE_TRANSFORM_CLASS(klass);
    object_class->set_property = gst_forge_set_property;
    object_class->get_property = gst_forge_get_property;
    object_class->finalize = gst_forge_finalize;
    properties[PROP_GAIN_DB] = g_param_spec_double(
        "gain-db", "Target gain (dB)", "Smoothed target gain in dB",
        -120.0, 120.0, 0.0, G_PARAM_READWRITE | G_PARAM_STATIC_STRINGS);
    properties[PROP_CEILING_DBTP] = g_param_spec_double(
        "ceiling-dbtp", "True-peak ceiling (dBTP)", "True-peak ceiling in dBTP",
        -120.0, 0.0, -1.0, G_PARAM_READWRITE | G_PARAM_STATIC_STRINGS);
    properties[PROP_ATTACK_MS] = g_param_spec_double(
        "attack-ms", "Attack (ms)", "Gain attack smoothing in milliseconds",
        0.01, 10000.0, 10.0, G_PARAM_READWRITE | G_PARAM_STATIC_STRINGS);
    properties[PROP_RELEASE_MS] = g_param_spec_double(
        "release-ms", "Release (ms)", "Gain release smoothing in milliseconds",
        0.01, 10000.0, 100.0, G_PARAM_READWRITE | G_PARAM_STATIC_STRINGS);
    g_object_class_install_properties(object_class, N_PROPERTIES, properties);

    gst_element_class_set_static_metadata(
        element_class,
        "Forge low-latency loudness gain processor",
        "Filter/Effect/Audio",
        "Interleaved F32LE five-millisecond look-ahead gain and true-peak limiter",
        "Forge Project <forge@example.invalid>");
    GstCaps *caps = gst_caps_from_string(
        "audio/x-raw,format=(string)F32LE,layout=(string)interleaved,"
        "rate=(int)[8000,384000],channels=(int)[1,64]");
    gst_audio_filter_class_add_pad_templates(audio_class, caps);
    gst_caps_unref(caps);
    audio_class->setup = gst_forge_setup;
    transform_class->transform_ip = gst_forge_transform_ip;
    transform_class->sink_event = gst_forge_sink_event;
    transform_class->query = gst_forge_query;
    transform_class->start = gst_forge_start;
    transform_class->stop = gst_forge_stop;
}

static void gst_forge_normalizer_init(GstForgeNormalizer *self) {
    self->gain_db = 0.0;
    self->ceiling_dbtp = -1.0;
    self->attack_ms = 10.0;
    self->release_ms = 100.0;
    self->last_end = GST_CLOCK_TIME_NONE;
    gst_base_transform_set_in_place(GST_BASE_TRANSFORM(self), TRUE);
}

static gboolean gst_forge_plugin_init(GstPlugin *plugin) {
    return gst_element_register(
        plugin, "forge_normalizer", GST_RANK_NONE, GST_TYPE_FORGE_NORMALIZER);
}

GST_PLUGIN_DEFINE(
    GST_VERSION_MAJOR,
    GST_VERSION_MINOR,
    forge,
    "Forge streaming loudness normalizer",
    gst_forge_plugin_init,
    FORGE_PLUGIN_VERSION,
    "MIT",
    "forge-normalizer",
    "https://github.com/penguin425/audio-normalizer")
