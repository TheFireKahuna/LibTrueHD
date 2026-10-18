/*
 * truehd_atmos_harness - verification harness for the LibTrueHD C API (macOS slice).
 *
 * Feeds a TrueHD stream to the decoder the way a player would: container input (.mka, .mkv,
 * .m2ts, .mp4) is demuxed with libavformat and pushed one AVPacket at a time; raw input (.thd,
 * .mlp) is pushed in pseudo-random chunks of 1..16384 bytes to exercise arbitrary chunking.
 * The push pts is the packet/chunk index, so every block can be mapped back to the input.
 *
 * Checks (exit status 1 on any failure):
 *   --ref-audio F     every decoded sample equals truehdd's CAF output (24-bit), in order
 *   --ref-metadata F  every object position change in truehdd's .atmos.metadata is matched by
 *                     an update here with the same position (see timing note in the report)
 *   --reset-at S --resume-at R
 *                     decode to S seconds, reset(), resume pushing at the packet holding R
 *                     seconds (a seek), then check the resumed output bit-exact against the
 *                     reference and input_access_unit against the uninterrupted pass
 */
#include <errno.h>
#include <inttypes.h>
#include <math.h>
#include <stdbool.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#include <libavformat/avformat.h>

#include "truehd_atmos.h"

_Static_assert(sizeof(TrueHDAtmosElement) == 64, "element layout");
_Static_assert(sizeof(TrueHDAtmosBlock) == 112, "block layout");
_Static_assert(sizeof(TrueHDAtmosStreamInfo) == 148, "stream info layout");

/* ---------------------------------------------------------------------------------------- */

typedef struct {
    uint8_t *data;
    size_t size;
} Packet;

typedef struct {
    Packet *items;
    size_t count, cap;
    bool container;
} Input;

static void input_add(Input *in, const uint8_t *data, size_t size) {
    if (in->count == in->cap) {
        in->cap = in->cap ? in->cap * 2 : 4096;
        in->items = realloc(in->items, in->cap * sizeof(Packet));
    }
    Packet *p = &in->items[in->count++];
    p->data = malloc(size ? size : 1);
    memcpy(p->data, data, size);
    p->size = size;
}

static bool ends_with(const char *s, const char *suffix) {
    size_t a = strlen(s), b = strlen(suffix);
    return a >= b && strcasecmp(s + a - b, suffix) == 0;
}

static uint64_t rng_state = 0x9E3779B97F4A7C15ull;
static uint64_t rng(void) {
    rng_state ^= rng_state << 13;
    rng_state ^= rng_state >> 7;
    rng_state ^= rng_state << 17;
    return rng_state;
}

static int load_input(const char *path, Input *in, size_t fixed_chunk) {
    if (ends_with(path, ".thd") || ends_with(path, ".mlp") || ends_with(path, ".truehd")) {
        FILE *f = fopen(path, "rb");
        if (!f) return -1;
        fseek(f, 0, SEEK_END);
        long n = ftell(f);
        fseek(f, 0, SEEK_SET);
        uint8_t *buf = malloc((size_t)n);
        if (fread(buf, 1, (size_t)n, f) != (size_t)n) return -1;
        fclose(f);
        for (size_t off = 0; off < (size_t)n;) {
            size_t len = fixed_chunk ? fixed_chunk : 1 + (size_t)(rng() % 16384);
            if (off + len > (size_t)n) len = (size_t)n - off;
            input_add(in, buf + off, len);
            off += len;
        }
        free(buf);
        in->container = false;
        return 0;
    }

    AVFormatContext *fmt = NULL;
    if (avformat_open_input(&fmt, path, NULL, NULL) < 0) return -1;
    if (avformat_find_stream_info(fmt, NULL) < 0) return -1;
    int stream = -1;
    for (unsigned i = 0; i < fmt->nb_streams; i++) {
        if (fmt->streams[i]->codecpar->codec_id == AV_CODEC_ID_TRUEHD) {
            stream = (int)i;
            break;
        }
    }
    if (stream < 0) {
        fprintf(stderr, "no TrueHD stream in %s\n", path);
        return -1;
    }
    AVPacket *pkt = av_packet_alloc();
    while (av_read_frame(fmt, pkt) >= 0) {
        if (pkt->stream_index == stream) input_add(in, pkt->data, (size_t)pkt->size);
        av_packet_unref(pkt);
    }
    av_packet_free(&pkt);
    avformat_close_input(&fmt);
    in->container = true;
    return 0;
}

/* ---------------------------------------------------------------------------------------- */
/* Reference audio: CAF as written by truehdd (24-bit integer PCM, interleaved).             */

typedef struct {
    int32_t *samples; /* interleaved */
    uint64_t frames;
    uint32_t channels;
} Reference;

static uint32_t be32(const uint8_t *p) { return (uint32_t)p[0] << 24 | p[1] << 16 | p[2] << 8 | p[3]; }
static uint64_t be64(const uint8_t *p) { return (uint64_t)be32(p) << 32 | be32(p + 4); }

static int load_caf(const char *path, Reference *ref) {
    FILE *f = fopen(path, "rb");
    if (!f) return -1;
    fseek(f, 0, SEEK_END);
    long n = ftell(f);
    fseek(f, 0, SEEK_SET);
    uint8_t *buf = malloc((size_t)n);
    if (fread(buf, 1, (size_t)n, f) != (size_t)n) return -1;
    fclose(f);
    if (n < 8 || memcmp(buf, "caff", 4) != 0) return -1;
    size_t pos = 8;
    bool little = false;
    uint32_t bits = 0;
    const uint8_t *data = NULL;
    uint64_t data_len = 0;
    while (pos + 12 <= (size_t)n) {
        const uint8_t *type = buf + pos;
        int64_t size = (int64_t)be64(buf + pos + 4);
        pos += 12;
        if (size < 0 || pos + (uint64_t)size > (size_t)n) size = (int64_t)((size_t)n - pos);
        if (memcmp(type, "desc", 4) == 0) {
            uint32_t flags = be32(buf + pos + 12);
            little = flags & 2;
            ref->channels = be32(buf + pos + 24);
            bits = be32(buf + pos + 28);
        } else if (memcmp(type, "data", 4) == 0) {
            data = buf + pos + 4; /* edit count */
            data_len = (uint64_t)size - 4;
        }
        pos += (size_t)size;
    }
    if (!data || bits != 24 || ref->channels == 0) return -1;
    uint64_t count = data_len / 3;
    ref->samples = malloc(count * sizeof(int32_t));
    for (uint64_t i = 0; i < count; i++) {
        const uint8_t *s = data + i * 3;
        int32_t v = little ? (s[0] | s[1] << 8 | s[2] << 16) : (s[0] << 16 | s[1] << 8 | s[2]);
        ref->samples[i] = (v << 8) >> 8;
    }
    ref->frames = count / ref->channels;
    free(buf);
    return 0;
}

typedef struct {
    uint64_t compared_frames, mismatched_samples, blocks_checked, wrapped_samples;
    int64_t max_abs_diff;
} Compare;

/* Compares a block with the reference at reference frame `at`. */
static void compare_block(const Reference *ref, const TrueHDAtmosBlock *b, uint64_t at, Compare *c) {
    if (!ref->samples) return;
    if (b->channel_count != ref->channels || at + b->frame_count > ref->frames) {
        c->mismatched_samples += (uint64_t)b->frame_count * b->channel_count;
        return;
    }
    for (uint32_t ch = 0; ch < b->channel_count; ch++) {
        const float *plane = b->channels[ch];
        for (uint32_t i = 0; i < b->frame_count; i++) {
            int32_t ours = (int32_t)lrintf(plane[i] * 8388608.0f);
            int32_t theirs = ref->samples[(at + i) * ref->channels + ch];
            /* truehdd writes 24-bit PCM and keeps the low 24 bits of a sample beyond that range;
               the library returns it unclipped. Such a sample matches if its low 24 bits do. */
            int32_t wrapped = (int32_t)((uint32_t)ours << 8) >> 8;
            bool beyond = wrapped != ours;
            if (beyond) c->wrapped_samples++;
            /* Also require the float to be exactly representable back (no rounding happened). */
            if (wrapped != theirs || (!beyond && (float)theirs / 8388608.0f != plane[i])) {
                c->mismatched_samples++;
                int64_t d = llabs((int64_t)ours - theirs);
                if (d > c->max_abs_diff) c->max_abs_diff = d;
            }
        }
    }
    c->compared_frames += b->frame_count;
    c->blocks_checked++;
}

/* ---------------------------------------------------------------------------------------- */
/* Reference metadata: truehdd .atmos.metadata events (ID, samplePos, pos).                  */

typedef struct {
    uint32_t id;
    uint64_t time;
    double pos[3];
} PosEvent;

typedef struct {
    PosEvent *items;
    size_t count, cap;
} PosEvents;

static void events_add(PosEvents *e, PosEvent v) {
    if (e->count == e->cap) {
        e->cap = e->cap ? e->cap * 2 : 1024;
        e->items = realloc(e->items, e->cap * sizeof(PosEvent));
    }
    e->items[e->count++] = v;
}

static int load_damf_events(const char *path, PosEvents *out) {
    FILE *f = fopen(path, "r");
    if (!f) return -1;
    char line[512];
    PosEvent cur = {0};
    bool open = false, have_pos = false;
    while (fgets(line, sizeof line, f)) {
        unsigned id;
        unsigned long long t;
        double x, y, z;
        if (sscanf(line, "  - ID: %u", &id) == 1) {
            if (open && have_pos) events_add(out, cur);
            memset(&cur, 0, sizeof cur);
            cur.id = id;
            open = true;
            have_pos = false;
        } else if (sscanf(line, "    samplePos: %llu", &t) == 1) {
            cur.time = t;
        } else if (sscanf(line, "    pos: [%lf, %lf, %lf]", &x, &y, &z) == 3) {
            cur.pos[0] = x;
            cur.pos[1] = y;
            cur.pos[2] = z;
            have_pos = true;
        }
    }
    if (open && have_pos) events_add(out, cur);
    fclose(f);
    return 0;
}

/* ---------------------------------------------------------------------------------------- */

typedef struct {
    int64_t ref_pos;   /* reference frame of the first access unit starting in this packet */
    int64_t ordinal;   /* input_access_unit of that access unit in the uninterrupted pass */
} PacketMap;

/* CPU time of this thread: what decoding costs, independent of other load on the machine. */
static double now_seconds(void) {
    struct timespec ts;
    clock_gettime(CLOCK_THREAD_CPUTIME_ID, &ts);
    return (double)ts.tv_sec + (double)ts.tv_nsec * 1e-9;
}

static const char *kind_name(uint8_t k) {
    switch (k) {
    case TRUEHD_ATMOS_ELEMENT_BED: return "bed";
    case TRUEHD_ATMOS_ELEMENT_LFE: return "lfe";
    case TRUEHD_ATMOS_ELEMENT_OBJECT: return "object";
    case TRUEHD_ATMOS_ELEMENT_ISF: return "isf";
    default: return "?";
    }
}

static const char *ptype_name(int32_t t) {
    switch (t) {
    case TRUEHD_ATMOS_PRESENTATION_INDEPENDENT: return "independent";
    case TRUEHD_ATMOS_PRESENTATION_DOWNMIX: return "downmix of";
    case TRUEHD_ATMOS_PRESENTATION_COPY: return "copy of";
    default: return "absent";
    }
}

static void print_stream_info(TrueHDAtmosDecoder *dec) {
    TrueHDAtmosStreamInfo si;
    if (truehd_atmos_decoder_get_stream_info(dec, &si) != TRUEHD_ATMOS_OK) {
        printf("stream info: not ready\n");
        return;
    }
    printf("stream: %s, %u Hz, %u frames/AU, %u substreams, immersive=%u objects=%u "
           "oamd_seen=%u selected=p%u layout_serial=%u\n",
           si.format == TRUEHD_ATMOS_FORMAT_TRUEHD ? "TrueHD (FBA)" : "MLP (FBB)", si.sample_rate,
           si.access_unit_frames, si.substream_count, si.immersive, si.has_objects, si.oamd_seen,
           si.selected_presentation, si.layout_serial);
    for (int p = 0; p < 4; p++) {
        const TrueHDAtmosPresentationInfo *pi = &si.presentations[p];
        printf("  p%d: %-11s", p, ptype_name(pi->type));
        if (pi->source >= 0) printf(" p%d", pi->source);
        if (pi->type != TRUEHD_ATMOS_PRESENTATION_ABSENT) {
            printf("  %2u ch:", pi->channel_count);
            for (uint32_t c = 0; c < pi->channel_count; c++)
                printf(" %s", truehd_atmos_speaker_name(pi->speakers[c]));
        }
        printf("\n");
    }
}

static void print_stats(TrueHDAtmosDecoder *dec, const char *label) {
    TrueHDAtmosStats s;
    truehd_atmos_decoder_get_stats(dec, &s);
    printf("%s: bytes=%" PRIu64 " pending=%" PRIu64 " AUs=%" PRIu64 " blocks=%" PRIu64
           " frames=%" PRIu64 " skipped=%" PRIu64 " dup=%" PRIu64 " decode_err=%" PRIu64
           " sync_err=%" PRIu64 " oamd=%" PRIu64 " oamd_err=%" PRIu64 " updates=%" PRIu64 "\n",
           label, s.bytes_pushed, s.bytes_pending, s.access_units_decoded, s.blocks_output,
           s.frames_output, s.access_units_skipped, s.duplicate_access_units, s.decode_errors,
           s.sync_errors, s.oamd_payloads, s.oamd_errors, s.metadata_updates);
    const char *err = truehd_atmos_decoder_last_error(dec);
    if (err && *err) printf("  last error: %s\n", err);
}

static int parse_presentation(const char *s) {
    if (!strcmp(s, "highest")) return TRUEHD_ATMOS_PRESENTATION_HIGHEST;
    if (!strcmp(s, "channel")) return TRUEHD_ATMOS_PRESENTATION_HIGHEST_CHANNEL_BASED;
    return atoi(s);
}

int main(int argc, char **argv) {
    const char *input_path = NULL, *ref_audio = NULL, *ref_meta = NULL;
    int presentation = TRUEHD_ATMOS_PRESENTATION_HIGHEST;
    double reset_at = -1, resume_at = -1;
    int watch[8] = {0, 1, 2};
    int watch_count = 3;
    size_t fixed_chunk = 0;
    bool summary = true;

    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--ref-audio") && i + 1 < argc) ref_audio = argv[++i];
        else if (!strcmp(argv[i], "--ref-metadata") && i + 1 < argc) ref_meta = argv[++i];
        else if (!strcmp(argv[i], "--presentation") && i + 1 < argc) presentation = parse_presentation(argv[++i]);
        else if (!strcmp(argv[i], "--reset-at") && i + 1 < argc) reset_at = atof(argv[++i]);
        else if (!strcmp(argv[i], "--resume-at") && i + 1 < argc) resume_at = atof(argv[++i]);
        else if (!strcmp(argv[i], "--chunk") && i + 1 < argc) fixed_chunk = (size_t)atol(argv[++i]);
        else if (!strcmp(argv[i], "--no-summary")) summary = false;
        else if (!strcmp(argv[i], "--objects") && i + 1 < argc) {
            watch_count = 0;
            for (char *tok = strtok(argv[++i], ","); tok && watch_count < 8; tok = strtok(NULL, ","))
                watch[watch_count++] = atoi(tok);
        } else if (argv[i][0] != '-') input_path = argv[i];
        else {
            fprintf(stderr, "unknown option %s\n", argv[i]);
            return 2;
        }
    }
    if (!input_path) {
        fprintf(stderr, "usage: %s [--presentation highest|channel|0..3] [--ref-audio caf] "
                        "[--ref-metadata atmos.metadata] [--reset-at s --resume-at s] "
                        "[--objects i,j,k] [--chunk bytes] input\n", argv[0]);
        return 2;
    }

    printf("%s (API %u)\n", truehd_atmos_version_string(), truehd_atmos_api_version());
    Input in = {0};
    if (load_input(input_path, &in, fixed_chunk) != 0) {
        fprintf(stderr, "cannot read %s\n", input_path);
        return 1;
    }
    uint64_t total_bytes = 0;
    for (size_t i = 0; i < in.count; i++) total_bytes += in.items[i].size;
    printf("input: %s, %zu %s, %" PRIu64 " bytes\n", input_path, in.count,
           in.container ? "demuxed packets (1 push each)" : (fixed_chunk ? "fixed-size chunks" : "random chunks of 1..16384 bytes"),
           total_bytes);

    Reference ref = {0};
    if (ref_audio && load_caf(ref_audio, &ref) != 0) {
        fprintf(stderr, "cannot read reference %s\n", ref_audio);
        return 1;
    }
    PosEvents cli_events = {0};
    if (ref_meta && load_damf_events(ref_meta, &cli_events) != 0) {
        fprintf(stderr, "cannot read reference %s\n", ref_meta);
        return 1;
    }

    bool failed = false;

    /* ---------------- Pass A: uninterrupted decode ---------------- */
    TrueHDAtmosDecoder *dec = truehd_atmos_decoder_create(presentation);
    if (!dec) return 1;
    PacketMap *map = malloc(in.count * sizeof(PacketMap));
    for (size_t i = 0; i < in.count; i++) map[i].ref_pos = map[i].ordinal = -1;

    Compare cmp = {0};
    double decode_time = 0;
    uint64_t blocks = 0, frames = 0, updates = 0, blocks_with_meta = 0;
    uint32_t rate = 0, first_serial = 0, serial_changes = 0, element_count = 0, bed_count = 0;
    uint32_t min_frames = UINT32_MAX, max_frames = 0, max_updates_per_block = 0;
    int64_t first_meta_frame = -1;
    uint64_t next_second = 0;
    bool info_printed = false, kinds_printed = false;
    uint32_t presentation_out = 0, channels_out = 0;
    /* Our position changes, for the metadata comparison. */
    PosEvents ours = {0};
    float last_target[16][3];
    bool have_target[16] = {0};
    uint64_t update_gap_hist[4] = {0}; /* frames between successive update starts: ==1536, other */
    int64_t last_update_time = -1;

    for (size_t k = 0; k < in.count; k++) {
        double t0 = now_seconds();
        TrueHDAtmosStatus st = truehd_atmos_decoder_push(dec, in.items[k].data, in.items[k].size, (int64_t)k);
        decode_time += now_seconds() - t0;
        if (st != TRUEHD_ATMOS_OK) {
            printf("push failed: %d\n", st);
            return 1;
        }
        for (;;) {
            TrueHDAtmosBlock b;
            t0 = now_seconds();
            st = truehd_atmos_decoder_pull(dec, &b);
            decode_time += now_seconds() - t0;
            if (st == TRUEHD_ATMOS_NEED_MORE_DATA) break;
            if (st != TRUEHD_ATMOS_OK) {
                printf("pull failed: %d\n", st);
                return 1;
            }
            if (!info_printed) {
                print_stream_info(dec);
                info_printed = true;
                rate = b.sample_rate;
                first_serial = b.layout_serial;
                presentation_out = b.presentation;
                channels_out = b.channel_count;
                printf("first block: p%u, %u ch, %u frames, flags=0x%x, pts=%" PRId64 "+%u AU, "
                       "input_access_unit=%" PRIu64 " sample_position=%" PRIu64 "\n",
                       b.presentation, b.channel_count, b.frame_count, b.flags, b.pts,
                       b.pts_au_index, b.input_access_unit, b.sample_position);
                printf("block speakers:");
                for (uint32_t c = 0; c < b.channel_count; c++) printf(" %s", truehd_atmos_speaker_name(b.speakers[c]));
                printf("\n");
            }
            if (b.layout_serial != first_serial) serial_changes++;
            if (b.pts >= 0 && (size_t)b.pts < in.count && map[b.pts].ref_pos < 0) {
                map[b.pts].ref_pos = (int64_t)b.sample_position - (int64_t)b.pts_au_index * b.access_unit_frames;
                map[b.pts].ordinal = (int64_t)b.input_access_unit - b.pts_au_index;
            }
            if (b.frame_count < min_frames) min_frames = b.frame_count;
            if (b.frame_count > max_frames) max_frames = b.frame_count;
            compare_block(&ref, &b, b.sample_position, &cmp);

            const TrueHDAtmosMetadata *md = b.metadata;
            if (md) {
                blocks_with_meta++;
                if (first_meta_frame < 0) first_meta_frame = (int64_t)b.sample_position;
                if (md->element_count != b.channel_count) {
                    printf("FAIL: metadata element_count %u != channel_count %u\n", md->element_count, b.channel_count);
                    failed = true;
                }
                for (uint32_t e = 0; e < md->element_count; e++) {
                    if (md->elements[e].speaker != b.speakers[e]) {
                        printf("FAIL: element %u speaker %u != block speaker %u\n", e, md->elements[e].speaker, b.speakers[e]);
                        failed = true;
                    }
                }
                if (!kinds_printed) {
                    element_count = md->element_count;
                    bed_count = md->bed_count;
                    printf("elements: %u (bed %u, isf %u, dynamic %u), warp_mode=%d\n", md->element_count,
                           md->bed_count, md->isf_count, md->dynamic_count, md->warp_mode);
                    printf("  kinds:");
                    for (uint32_t e = 0; e < md->element_count; e++) {
                        const TrueHDAtmosElement *el = &md->elements[e];
                        printf(" %u:%s", e, kind_name(el->kind));
                        if (el->kind == TRUEHD_ATMOS_ELEMENT_BED || el->kind == TRUEHD_ATMOS_ELEMENT_LFE)
                            printf("(%s)", truehd_atmos_speaker_name(el->speaker));
                    }
                    printf("\n");
                    kinds_printed = true;
                }
                if (md->update_count > max_updates_per_block) max_updates_per_block = md->update_count;
                for (uint32_t u = 0; u < md->update_count; u++) {
                    const TrueHDAtmosMetadataUpdate *up = &md->updates[u];
                    if (up->frame_offset >= b.frame_count) {
                        printf("FAIL: update frame_offset %u outside block of %u\n", up->frame_offset, b.frame_count);
                        failed = true;
                    }
                    int64_t t = (int64_t)(b.sample_position + up->frame_offset);
                    if (last_update_time >= 0) update_gap_hist[(t - last_update_time) == 1536 ? 0 : 1]++;
                    last_update_time = t;
                    updates++;
                    for (uint32_t e = 0; e < md->element_count && e < 16; e++) {
                        const TrueHDAtmosElement *el = &up->elements[e];
                        if (el->kind != TRUEHD_ATMOS_ELEMENT_OBJECT) continue;
                        bool moved = !have_target[e] || memcmp(last_target[e], el->position, sizeof last_target[e]) != 0;
                        if (moved) {
                            if (!(el->flags & TRUEHD_ATMOS_ELEMENT_CHANGED)) {
                                printf("FAIL: element %u moved without CHANGED\n", e);
                                failed = true;
                            }
                            PosEvent pe = {.id = e, .time = (uint64_t)t};
                            /* to truehdd's DAMF convention */
                            pe.pos[0] = 2.0 * el->position[0] - 1.0;
                            pe.pos[1] = 1.0 - 2.0 * el->position[1];
                            pe.pos[2] = el->position[2];
                            events_add(&ours, pe);
                            memcpy(last_target[e], el->position, sizeof last_target[e]);
                            have_target[e] = true;
                        }
                    }
                }
            }

            /* Per-second motion summary of the watched dynamic objects. */
            if (summary && md && b.sample_position >= next_second * rate) {
                printf("t=%3" PRIu64 "s", next_second);
                uint32_t shown = 0;
                for (int w = 0; w < watch_count; w++) {
                    uint32_t e = md->bed_count + md->isf_count + (uint32_t)watch[w];
                    if (e >= md->element_count) continue;
                    const TrueHDAtmosElement *el = &md->elements[e];
                    printf("  obj%d[x=%.3f y=%.3f z=%.3f g=%5.1fdB%s%s]", watch[w], el->position[0],
                           el->position[1], el->position[2], el->gain_db,
                           el->flags & TRUEHD_ATMOS_ELEMENT_ACTIVE ? "" : " off",
                           el->size[0] > 0 ? " sized" : "");
                    shown++;
                }
                if (!shown) printf("  (no such dynamic objects)");
                printf("\n");
                next_second++;
            } else if (summary && !md && rate && b.sample_position >= next_second * rate) {
                next_second++;
            }

            blocks++;
            frames += b.frame_count;
        }
    }
    double seconds = rate ? (double)frames / rate : 0;
    printf("pass A: %" PRIu64 " blocks, %" PRIu64 " frames (%.3f s), block frames %u..%u, p%u %u ch\n",
           blocks, frames, seconds, min_frames, max_frames, presentation_out, channels_out);
    printf("pass A: decode CPU time %.3f s -> %.1fx realtime (push+pull only, one thread's CPU "
           "time)\n", decode_time, decode_time > 0 ? seconds / decode_time : 0);
    printf("pass A: layout_serial constant: %s (%u changes)\n", serial_changes ? "NO" : "yes", serial_changes);
    if (serial_changes) failed = true;
    if (blocks_with_meta) {
        printf("pass A: metadata on %" PRIu64 "/%" PRIu64 " blocks (first at frame %" PRId64 "), %" PRIu64
               " updates, max %u per block, update spacing ==1536: %" PRIu64 " other: %" PRIu64 "\n",
               blocks_with_meta, blocks, first_meta_frame, updates, max_updates_per_block,
               update_gap_hist[0], update_gap_hist[1]);
    }
    print_stats(dec, "pass A stats");

    if (ref.samples) {
        bool ok = cmp.mismatched_samples == 0 && cmp.compared_frames == ref.frames;
        printf("PCM vs truehdd: %s - %" PRIu64 "/%" PRIu64 " frames compared, %" PRIu64
               " mismatched samples (max |diff| %" PRId64 "), %" PRIu64 " beyond 24 bits\n",
               ok ? "BIT-EXACT" : "MISMATCH", cmp.compared_frames, ref.frames, cmp.mismatched_samples,
               cmp.max_abs_diff, cmp.wrapped_samples);
        if (!ok) failed = true;
    }

    if (ref_meta) {
        /* Match truehdd position events to ours: same element, same DAMF position, and a start
           time equal to truehdd's samplePos or 32 frames later (truehdd leaves out the
           32 * block_offset_factor term of the update time). */
        uint64_t matched = 0, exact_time = 0, plus32 = 0, unmatched = 0;
        size_t j = 0;
        for (size_t i = 0; i < cli_events.count; i++) {
            const PosEvent *c = &cli_events.items[i];
            if (c->id < 10) continue; /* bed channels carry no position */
            uint32_t element = c->id - 10 + bed_count;
            bool found = false;
            while (j < ours.count && ours.items[j].time + 64 < c->time) j++;
            for (size_t k = j; k < ours.count && ours.items[k].time <= c->time + 64; k++) {
                const PosEvent *o = &ours.items[k];
                if (o->id != element) continue;
                int64_t dt = (int64_t)o->time - (int64_t)c->time;
                if ((dt == 0 || dt == 32) && fabs(o->pos[0] - c->pos[0]) < 1e-6 &&
                    fabs(o->pos[1] - c->pos[1]) < 1e-6 && fabs(o->pos[2] - c->pos[2]) < 1e-6) {
                    found = true;
                    if (dt == 0) exact_time++;
                    else plus32++;
                    break;
                }
            }
            if (found) matched++;
            else {
                if (unmatched < 5)
                    printf("  unmatched truehdd event: ID %u t=%" PRIu64 " pos [%g, %g, %g]\n", c->id,
                           c->time, c->pos[0], c->pos[1], c->pos[2]);
                unmatched++;
            }
        }
        size_t cli_objects = 0;
        for (size_t i = 0; i < cli_events.count; i++) cli_objects += cli_events.items[i].id >= 10;
        bool ok = unmatched == 0 && cli_objects == ours.count;
        printf("OAMD vs truehdd: %s - %zu truehdd position events, %zu ours; matched %" PRIu64
               " (same time %" PRIu64 ", +32 frames %" PRIu64 "), unmatched %" PRIu64 "\n",
               ok ? "MATCH" : "MISMATCH", cli_objects, ours.count, matched, exact_time, plus32, unmatched);
        (void)element_count;
        if (!ok) failed = true;
    }
    truehd_atmos_decoder_destroy(dec);

    /* ---------------- Pass B: reset (seek) mid-stream ---------------- */
    if (reset_at >= 0 && rate) {
        if (resume_at < 0) resume_at = reset_at;
        uint64_t reset_frame = (uint64_t)(reset_at * rate), resume_frame = (uint64_t)(resume_at * rate);
        size_t resume_packet = 0;
        for (size_t k = 0; k < in.count; k++) {
            if (map[k].ref_pos >= 0 && (uint64_t)map[k].ref_pos >= resume_frame) {
                resume_packet = k;
                break;
            }
        }
        /* Raw chunks may start mid-access-unit; resume at the chunk anyway (exercises the
           estimated path) but measure against the chunk's first access unit. */
        printf("pass B: decode to %.2f s, reset(), resume at packet %zu (reference frame %" PRId64
               ", %.3f s)\n", reset_at, resume_packet, map[resume_packet].ref_pos,
               (double)map[resume_packet].ref_pos / rate);

        dec = truehd_atmos_decoder_create(presentation);
        Compare before = {0}, after = {0};
        bool reset_done = false, first_after = true;
        uint64_t after_blocks = 0, ordinal_mismatch = 0, estimated_blocks = 0;
        int64_t first_meta_after = -1;
        size_t k = 0;
        while (k < in.count) {
            truehd_atmos_decoder_push(dec, in.items[k].data, in.items[k].size, (int64_t)k);
            k++;
            for (;;) {
                TrueHDAtmosBlock b;
                TrueHDAtmosStatus st = truehd_atmos_decoder_pull(dec, &b);
                if (st != TRUEHD_ATMOS_OK) break;
                if (b.pts < 0 || map[b.pts].ref_pos < 0) {
                    printf("FAIL: block without packet mapping (pts %" PRId64 ")\n", b.pts);
                    failed = true;
                    continue;
                }
                uint64_t at = (uint64_t)map[b.pts].ref_pos + (uint64_t)b.pts_au_index * b.access_unit_frames;
                if (!reset_done) {
                    compare_block(&ref, &b, at, &before);
                } else {
                    int64_t expected_ordinal = map[b.pts].ordinal + b.pts_au_index - map[resume_packet].ordinal;
                    if (first_after) {
                        printf("pass B: first block after reset: flags=0x%x%s%s, sample_position=%" PRIu64
                               ", input_access_unit=%" PRIu64 " (expected %" PRId64 "), input_frame_offset=%" PRIu64
                               " = %.2f ms after the resume packet, reference frame %" PRIu64 "\n",
                               b.flags, b.flags & TRUEHD_ATMOS_BLOCK_DISCONTINUITY ? " DISCONTINUITY" : "",
                               b.flags & TRUEHD_ATMOS_BLOCK_INPUT_OFFSET_ESTIMATED ? " ESTIMATED" : "",
                               b.sample_position, b.input_access_unit, expected_ordinal, b.input_frame_offset,
                               1000.0 * (double)b.input_frame_offset / rate, at);
                        if (!(b.flags & TRUEHD_ATMOS_BLOCK_DISCONTINUITY) || b.sample_position != 0) {
                            printf("FAIL: first block after reset lacks DISCONTINUITY or position 0\n");
                            failed = true;
                        }
                        first_after = false;
                    }
                    if (b.flags & TRUEHD_ATMOS_BLOCK_INPUT_OFFSET_ESTIMATED) estimated_blocks++;
                    else if ((int64_t)b.input_access_unit != expected_ordinal) ordinal_mismatch++;
                    /* Timeline check: resume packet's reference position + input_frame_offset
                       must be this block's reference position. */
                    if (!(b.flags & TRUEHD_ATMOS_BLOCK_INPUT_OFFSET_ESTIMATED) &&
                        (uint64_t)map[resume_packet].ref_pos + b.input_frame_offset != at)
                        ordinal_mismatch++;
                    compare_block(&ref, &b, at, &after);
                    if (first_meta_after < 0 && b.metadata) first_meta_after = (int64_t)b.sample_position;
                    after_blocks++;
                }
                if (!reset_done && b.sample_position + b.frame_count >= reset_frame) {
                    truehd_atmos_decoder_reset(dec);
                    reset_done = true;
                    k = resume_packet;
                    break;
                }
            }
        }
        print_stats(dec, "pass B stats (since reset)");
        bool ok = after_blocks > 0 && after.mismatched_samples == 0 && before.mismatched_samples == 0 &&
                  ordinal_mismatch == 0;
        printf("pass B: %s - before reset %" PRIu64 " frames bit-exact=%s; after reset %" PRIu64
               " blocks, %" PRIu64 " frames bit-exact=%s, input timeline %s%s\n",
               ok ? "OK" : "FAIL", before.compared_frames, before.mismatched_samples ? "no" : "yes",
               after_blocks, after.compared_frames, after.mismatched_samples ? "no" : "yes",
               ordinal_mismatch ? "MISMATCH" : "exact", estimated_blocks ? " (estimated: resume was mid-access-unit)" : "");
        if (first_meta_after >= 0)
            printf("pass B: object metadata available from output frame %" PRId64 " after the reset "
                   "(%.1f ms after the first decoded block)\n", first_meta_after,
                   1000.0 * (double)first_meta_after / rate);
        if (!ref.samples) printf("pass B: (no --ref-audio: PCM not compared)\n");
        if (!ok) failed = true;
        truehd_atmos_decoder_destroy(dec);
    }

    printf("%s\n", failed ? "RESULT: FAIL" : "RESULT: PASS");
    return failed ? 1 : 0;
}
