/*
 * truehd_atmos.h - C API of the LibTrueHD decoder library.
 *
 * Decodes Dolby TrueHD (MLP FBA, and MLP FBB) bitstreams to planar float PCM, including the
 * Dolby Atmos object presentation (presentation 3) together with its object audio metadata
 * (OAMD): per-element kind, speaker, 3D position, gain, size and render flags, with
 * sample-accurate update timing. Built on the Rust `truehd` crate from
 * https://github.com/truehdd/truehdd (Apache-2.0).
 *
 * Model
 *   create -> [push bytes -> pull blocks until NEED_MORE_DATA]* -> destroy
 *   - push() accepts any chunking: whole demuxer packets (one or more access units each),
 *     partial access units, or arbitrary byte ranges of an elementary stream.
 *   - pull() decodes one access unit per call and returns it as one block.
 *   - reset() is for seeks: it drops all buffered input and decoder state; decoding resumes
 *     at the next major sync in the data pushed afterwards.
 *
 * Threading
 *   A decoder handle is not thread-safe: call it from one thread at a time. Separate handles
 *   are independent. No function blocks or performs I/O.
 *
 * Memory
 *   All pointers returned inside TrueHDAtmosBlock (channel planes, speakers, metadata and
 *   everything it points to) are owned by the decoder and stay valid until the next call to
 *   truehd_atmos_decoder_pull(), _reset(), _set_presentation() or _destroy() on the same
 *   handle. push(), get_stream_info(), get_trims(), get_stats() and last_error() do not
 *   invalidate them.
 *   Output buffers, metadata and bookkeeping are preallocated per handle. What pull() does
 *   allocate per access unit is the upstream `truehd` parser/decoder's working data (about 11
 *   allocations / 150 KB for a 16-channel access unit, 7 / 120 KB for 8 channels, freed again
 *   within the call) plus one copy of the access unit bytes (~0.5-1 KB) that its API takes.
 *
 * Errors
 *   Functions return TrueHDAtmosStatus. Damaged or undecodable input never makes pull() fail:
 *   the decoder drops data up to the next major sync, counts the event in
 *   TrueHDAtmosStats, records a message for truehd_atmos_decoder_last_error(), and flags the
 *   next block with TRUEHD_ATMOS_BLOCK_DISCONTINUITY. Rust panics never cross this API; if
 *   one is caught outside the per-access-unit recovery, the handle returns
 *   TRUEHD_ATMOS_ERR_PANIC until truehd_atmos_decoder_reset() or _destroy().
 */
#ifndef TRUEHD_ATMOS_H
#define TRUEHD_ATMOS_H

#include <stddef.h>
#include <stdbool.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/*
 * Constants are macros with the type of the field or parameter they go with, so they stay
 * constant expressions in C and import into Swift with that type (UInt8, UInt32, Int32),
 * comparable without conversions.
 */

#define TRUEHD_ATMOS_API_VERSION 3u

/** Most channels (elements) any presentation can carry. */
#define TRUEHD_ATMOS_MAX_CHANNELS 16u
/** Most frames one block can hold (one access unit at 176.4/192 kHz). */
#define TRUEHD_ATMOS_MAX_BLOCK_FRAMES 160u
/** "No timestamp" for push() and TrueHDAtmosBlock.pts: INT64_MIN, the same value as FFmpeg's
    AV_NOPTS_VALUE, so AVPacket.pts can be passed as is. Swift does not import this macro;
    use Int64.min. */
#define TRUEHD_ATMOS_NO_PTS INT64_MIN

/* ------------------------------------------------------------------------------------------ */
/* Status codes                                                                               */

typedef int32_t TrueHDAtmosStatus;

#define TRUEHD_ATMOS_OK ((TrueHDAtmosStatus)0)
/** pull(): no complete access unit is buffered; push more data (or stop at end of stream). */
#define TRUEHD_ATMOS_NEED_MORE_DATA ((TrueHDAtmosStatus)1)
/** A required pointer argument was NULL. */
#define TRUEHD_ATMOS_ERR_NULL ((TrueHDAtmosStatus)-1)
/** An argument was out of range (for example an unknown presentation selector). */
#define TRUEHD_ATMOS_ERR_INVALID_ARGUMENT ((TrueHDAtmosStatus)-2)
/** get_stream_info(): no major sync has been decoded since create(). */
#define TRUEHD_ATMOS_ERR_NOT_READY ((TrueHDAtmosStatus)-3)
/** push(): more than 8 MiB of input is waiting; pull() before pushing more. Nothing was consumed. */
#define TRUEHD_ATMOS_ERR_BUFFER_FULL ((TrueHDAtmosStatus)-4)
/** An internal panic was caught at the API boundary; call reset() or destroy(). */
#define TRUEHD_ATMOS_ERR_PANIC ((TrueHDAtmosStatus)-5)

/* ------------------------------------------------------------------------------------------ */
/* Presentation selection                                                                     */
/*
 * A TrueHD stream carries up to four presentations of the same programme:
 *   0: 2 channels, 1: up to 6 channels (5.1), 2: up to 8 channels (7.1),
 *   3: up to 16 channels - with Atmos this is the object presentation (bed + dynamic
 *      objects + OAMD); without objects it is a channel-based immersive bed (e.g. 9.1.6).
 * A selector naming a presentation the stream does not carry falls back to the highest
 * presentation it does carry (and a presentation that is a copy of a lower one decodes that
 * one); TrueHDAtmosBlock.presentation always reports what was actually decoded.
 */
/** Presentation 3 when present (Atmos objects or 16-channel bed), else the highest available. */
#define TRUEHD_ATMOS_PRESENTATION_HIGHEST ((int32_t)-1)
/** Highest of presentations 0..2: the 7.1 (or 5.1/2.0) channel presentation, never objects. */
#define TRUEHD_ATMOS_PRESENTATION_HIGHEST_CHANNEL_BASED ((int32_t)-2)
/* 0, 1, 2, 3: that presentation explicitly. */

/* ------------------------------------------------------------------------------------------ */
/* Speakers                                                                                   */
/*
 * Channel/speaker codes used for bed channels of every presentation. Codes 0..23 follow the
 * TrueHD channel assignment labels; the OAMD bed labels map onto them as noted.
 */
#define TRUEHD_ATMOS_SPEAKER_L ((uint8_t)0)
#define TRUEHD_ATMOS_SPEAKER_R ((uint8_t)1)
#define TRUEHD_ATMOS_SPEAKER_C ((uint8_t)2)
#define TRUEHD_ATMOS_SPEAKER_LFE ((uint8_t)3)
#define TRUEHD_ATMOS_SPEAKER_LS ((uint8_t)4)  /* left surround / side surround (OAMD Lss) */
#define TRUEHD_ATMOS_SPEAKER_RS ((uint8_t)5)  /* right surround / side surround (OAMD Rss) */
#define TRUEHD_ATMOS_SPEAKER_TFL ((uint8_t)6)  /* top front left (OAMD Lfh) */
#define TRUEHD_ATMOS_SPEAKER_TFR ((uint8_t)7)  /* top front right (OAMD Rfh) */
#define TRUEHD_ATMOS_SPEAKER_TSL ((uint8_t)8)  /* top side/middle left (OAMD Lts) */
#define TRUEHD_ATMOS_SPEAKER_TSR ((uint8_t)9)  /* top side/middle right (OAMD Rts) */
#define TRUEHD_ATMOS_SPEAKER_TBL ((uint8_t)10)  /* top back left (OAMD Lrh) */
#define TRUEHD_ATMOS_SPEAKER_TBR ((uint8_t)11)  /* top back right (OAMD Rrh) */
#define TRUEHD_ATMOS_SPEAKER_LSC ((uint8_t)12)  /* left of centre, screen (Lc) */
#define TRUEHD_ATMOS_SPEAKER_RSC ((uint8_t)13)  /* right of centre, screen (Rc) */
#define TRUEHD_ATMOS_SPEAKER_LB ((uint8_t)14)  /* left back / rear surround (OAMD Lrs) */
#define TRUEHD_ATMOS_SPEAKER_RB ((uint8_t)15)  /* right back / rear surround (OAMD Rrs) */
#define TRUEHD_ATMOS_SPEAKER_CB ((uint8_t)16)  /* centre back / centre surround */
#define TRUEHD_ATMOS_SPEAKER_TC ((uint8_t)17)  /* top centre (overhead) */
#define TRUEHD_ATMOS_SPEAKER_LSD ((uint8_t)18)  /* left surround direct */
#define TRUEHD_ATMOS_SPEAKER_RSD ((uint8_t)19)  /* right surround direct */
#define TRUEHD_ATMOS_SPEAKER_LW ((uint8_t)20)  /* left wide (OAMD Lw) */
#define TRUEHD_ATMOS_SPEAKER_RW ((uint8_t)21)  /* right wide (OAMD Rw) */
#define TRUEHD_ATMOS_SPEAKER_TFC ((uint8_t)22)  /* top front centre */
#define TRUEHD_ATMOS_SPEAKER_LFE2 ((uint8_t)23)
/** The channel is an object (dynamic or ISF), not a speaker feed. */
#define TRUEHD_ATMOS_SPEAKER_OBJECT ((uint8_t)254)
/** The stream does not state this channel's speaker. */
#define TRUEHD_ATMOS_SPEAKER_UNKNOWN ((uint8_t)255)

/* ------------------------------------------------------------------------------------------ */
/* Object audio metadata                                                                      */
/*
 * COORDINATES (TrueHDAtmosElement.position) are the OAMD room-anchored coordinates of
 * ETSI TS 103 420 clause 4.2.1, unmodified:
 *   x: 0 = left wall      .. 1 = right wall
 *   y: 0 = front wall     .. 1 = back wall            (the screen is at y = 0)
 *   z: -1 = floor .. 0 = listener (ear-level) plane .. +1 = ceiling
 * Ear-level bed speakers sit at z = 0 and height speakers at z = 1; Atmos home content keeps
 * objects in z in [0, 1]. Values are clamped to x,y in [0,1], z in [-1,1] with the extended
 * precision refinement applied (resolution 1/310 in x,y and 1/75 in z).
 * Converting to the ADM / Dolby Atmos Master (DAMF) cartesian convention used by `truehdd`'s
 * .atmos.metadata files (x -1 left..+1 right, y +1 front..-1 back, z 0 ear..1 top):
 *   X = 2x - 1,  Y = 1 - 2y,  Z = z.
 * That holds for room-anchored positions only (TS 103 420 Annex B). When
 * TRUEHD_ATMOS_ELEMENT_SCREEN_REF is set, x and z are screen-anchored (0..1 across the screen
 * width, -1..1 over its height) and screen_factor/depth_factor say how much of the screen
 * reference to apply (TS 103 420 clause 5.2.1.3): resolve that to a room position first. An
 * element with TRUEHD_ATMOS_ELEMENT_DISTANCE lies outside the room along the ray through its
 * position (5.2.1.2), which the conversion above does not carry.
 *
 * Bed channels (kind BED/LFE) carry their nominal speaker position in the same coordinates
 * (LFE: 0,0,-1), their gain and ACTIVE flag; the other render fields are zero.
 *
 * TIMING: an OAMD payload arrives with an access unit (every 1536 samples = 32 ms in the
 * streams tested, i.e. every 38-39 access units at 48 kHz) and states one or more updates.
 * Update n starts at (access unit start + sample_offset + 32 * block offset factor(n)) per
 * TS 103 420 clause 5.3, which can be in a later access unit than the one that carried it.
 * The EMDF container's smploffst (TS 102 366 H.2.2.3.2) states the same offset, so it is not
 * added; a container that states a different one is counted in
 * TrueHDAtmosStats.oamd_offset_mismatches. The decoder delivers each update in the block that
 * contains its start frame: TrueHDAtmosMetadataUpdate.frame_offset is always <
 * TrueHDAtmosBlock.frame_count.
 * Rendering rule (TS 103 448 clause 5.5, pseudocode 19): at frame_offset, start a linear
 * interpolation of every element, from its current (possibly mid-ramp) gains to the update's
 * over ramp_frames + 1 frames (0 = jump). Every update restarts every element's ramp, the
 * elements it restates unchanged included: TRUEHD_ATMOS_ELEMENT_CHANGED only says the values
 * differ from the previous update's.
 * Every payload restates every element, so the first update after create/reset/DISCONTINUITY
 * describes every element (all CHANGED): apply it as a jump, there is nothing to ramp from.
 * Two things do carry over from earlier payloads, and the decoder resolves both: a divergence
 * coded as "the previous block's" (object_div_mode 1), and the trim element, which stays in
 * force until another arrives (TrueHDAtmosMetadataUpdate.trims and .warp_mode). Trims are per
 * metadata update (TS 103 448 table 2): a payload carrying them without object data has no update
 * of its own, so they take effect with the next update.
 * Blocks before the first update have no metadata (TRUEHD_ATMOS_BLOCK_HAS_METADATA clear) - at
 * most one payload interval, typically 32 ms - during which object channels carry audio with
 * no known position; mute or hold them.
 */

/* TrueHDAtmosElement.kind */
#define TRUEHD_ATMOS_ELEMENT_BED ((uint8_t)0)     /* speaker-anchored bed channel; see .speaker */
#define TRUEHD_ATMOS_ELEMENT_LFE ((uint8_t)1)     /* LFE or LFE2 bed channel */
#define TRUEHD_ATMOS_ELEMENT_OBJECT ((uint8_t)2)  /* dynamic object, room- or screen-anchored position */
#define TRUEHD_ATMOS_ELEMENT_ISF ((uint8_t)3)     /* intermediate spatial format object; position not decoded */

/* TrueHDAtmosElement.flags */
#define TRUEHD_ATMOS_ELEMENT_ACTIVE 0x001u             /* object active (not b_object_not_active) */
#define TRUEHD_ATMOS_ELEMENT_CHANGED 0x002u            /* in an update: differs from the previous values */
#define TRUEHD_ATMOS_ELEMENT_SNAP 0x004u               /* snap to the nearest speaker (b_object_snap) */
#define TRUEHD_ATMOS_ELEMENT_ELEVATION 0x008u          /* top/bottom zone allowed (b_enable_elevation) */
#define TRUEHD_ATMOS_ELEMENT_SCREEN_REF 0x010u         /* position is screen-anchored */
#define TRUEHD_ATMOS_ELEMENT_DISTANCE 0x020u           /* .distance is specified */
#define TRUEHD_ATMOS_ELEMENT_DIVERGENCE 0x040u         /* .divergence is specified */
#define TRUEHD_ATMOS_ELEMENT_TRIM_BYPASS 0x080u        /* downmix trims disabled for this element */
#define TRUEHD_ATMOS_ELEMENT_HEAD_TRACK_DISABLE 0x100u /* headphone head tracking disabled */

/* TrueHDAtmosElement.dialog: the element is dialogue. */
#define TRUEHD_ATMOS_DIALOG_DIALOGUE ((uint8_t)1)

/* TrueHDAtmosElement.zone: horizontal zone constraint (TS 103 420 table 20; the reserved 6 and 7
 * arrive as ALL). */
#define TRUEHD_ATMOS_ZONE_ALL ((uint8_t)0)
#define TRUEHD_ATMOS_ZONE_NO_BACK ((uint8_t)1)
#define TRUEHD_ATMOS_ZONE_NO_SIDES ((uint8_t)2)
#define TRUEHD_ATMOS_ZONE_CENTER_BACK ((uint8_t)3)  /* centre and back zones only */
#define TRUEHD_ATMOS_ZONE_SCREEN_ONLY ((uint8_t)4)
#define TRUEHD_ATMOS_ZONE_SURROUND_ONLY ((uint8_t)5)

/* TrueHDAtmosMetadata.warp_mode (OAMD trim element, TS 103 420 table 32). WARPING: every object's
 * y shall be multiplied by 2 before rendering. 2 and 3 are reserved in TS 103 420; the names are
 * the Dolby Atmos Master (DAMF) ones truehdd writes for them. */
#define TRUEHD_ATMOS_WARP_NOT_SIGNALLED ((int32_t)-1)
#define TRUEHD_ATMOS_WARP_NORMAL ((int32_t)0)
#define TRUEHD_ATMOS_WARP_WARPING ((int32_t)1)
#define TRUEHD_ATMOS_WARP_PROLOGIC_IIX ((int32_t)2)
#define TRUEHD_ATMOS_WARP_LORO ((int32_t)3)

typedef struct TrueHDAtmosElement {
    uint8_t kind;            /* TRUEHD_ATMOS_ELEMENT_BED/LFE/OBJECT/ISF */
    uint8_t speaker;         /* bed/LFE: TRUEHD_ATMOS_SPEAKER_*; objects: TRUEHD_ATMOS_SPEAKER_OBJECT */
    uint8_t zone;            /* TRUEHD_ATMOS_ZONE_* (objects) */
    uint8_t headphone_mode;  /* headphone render mode: 0 bypass, 1 near, 2 far, 3 mid; 255 = not signalled */
    uint32_t flags;          /* TRUEHD_ATMOS_ELEMENT_* */
    float position[3];       /* x, y, z; see COORDINATES above */
    float gain;              /* linear amplitude gain, 0 for -inf dB (range -inf, -49..+15 dB) */
    float gain_db;           /* same gain in dB, -INFINITY when muted */
    float size[3];           /* width, depth, height in 0..1 of the room (0 = point source) */
    float divergence;        /* 0..1 when TRUEHD_ATMOS_ELEMENT_DIVERGENCE, else 0 */
    float priority;          /* 0..1 importance (1 = default/highest) */
    float distance;          /* when TRUEHD_ATMOS_ELEMENT_DISTANCE: distance_factor, the object's distance
                                over the room boundary's along the same ray (1.1..50.1, INFINITY = at
                                infinity; TS 103 420 5.2.1.2), else 0 */
    float screen_factor;     /* 0.125..1 when TRUEHD_ATMOS_ELEMENT_SCREEN_REF, else 0 */
    float depth_factor;      /* 0.25, 0.5, 1 or 2 */
    uint8_t dialog;          /* OAMD object description (element 4, reserved in TS 103 420): 1 dialogue,
                                2 not dialogue, 3 with dialog_extra, 0 nothing stated; 255 = no element */
    uint8_t dialog_extra;    /* the 5 bits that come with dialog 3, meaning unknown; 255 = none */
    uint8_t reserved[2];
} TrueHDAtmosElement;        /* 64 bytes */

struct TrueHDAtmosTrim;

typedef struct TrueHDAtmosMetadataUpdate {
    uint32_t frame_offset;               /* first frame of this block the update applies from */
    uint32_t ramp_frames;                /* interpolation length in frames, 0 = jump */
    const TrueHDAtmosElement *elements;  /* element_count entries: the values reached after the ramp */
    const struct TrueHDAtmosTrim *trims; /* TRUEHD_ATMOS_TRIM_CONFIGS entries: the trims in force for
                                            this update (TS 103 448 table 2: per metadata update) */
    int32_t warp_mode;                   /* TRUEHD_ATMOS_WARP_*: the warp mode in force for this update */
    uint32_t reserved;
} TrueHDAtmosMetadataUpdate;            /* 32 bytes */

/* TrueHDAtmosTrim.mode */
#define TRUEHD_ATMOS_TRIM_DEFAULT ((uint8_t)0)   /* the renderer's own trim (TS 103 448 pseudocode 16) */
#define TRUEHD_ATMOS_TRIM_DISABLED ((uint8_t)1)  /* no trim */
#define TRUEHD_ATMOS_TRIM_CUSTOM ((uint8_t)2)    /* the gains below (TS 103 448 pseudocode 17) */

/** Trim configurations, indexed by TS 103 448 trim_cfg (table 7: 3 x heights + surrounds). */
#define TRUEHD_ATMOS_TRIM_CONFIGS 9u

typedef struct TrueHDAtmosTrim {
    uint8_t mode;            /* TRUEHD_ATMOS_TRIM_* */
    uint8_t reserved[3];
    float centre_db;         /* custom trims: trim_centre, trim_surround, trim_height in dB; 0 if */
    float surround_db;       /* not transmitted, and for the other modes */
    float height_db;
    float balance_top;       /* custom trims: bal3D_Y_tb and bal3D_Y_lis (TS 103 420 5.6.5.9-12), */
    float balance_listener;  /* front/back balance in the top and bottom planes and in the
                                listener plane: -1..1, negative towards the front; 0 if not
                                transmitted (TS 103 448 does not say how to render it) */
} TrueHDAtmosTrim;           /* 24 bytes */

typedef struct TrueHDAtmosMetadata {
    uint32_t element_count;   /* == TrueHDAtmosBlock.channel_count; element i is channel i */
    uint32_t bed_count;       /* elements [0, bed_count) are bed channels (incl. LFE) */
    uint32_t isf_count;       /* then isf_count ISF objects */
    uint32_t dynamic_count;   /* then dynamic objects up to element_count */
    int32_t warp_mode;        /* TRUEHD_ATMOS_WARP_*: that of the latest update started at or before
                                 this block's last frame (from the trim element of its payload) */
    uint32_t update_count;    /* updates starting inside this block (usually 0 or 1) */
    const TrueHDAtmosMetadataUpdate *updates;  /* update_count entries, ascending frame_offset */
    const TrueHDAtmosElement *elements;        /* element_count entries: the values of the latest
                                                  update started at or before this block's last
                                                  frame (CHANGED bits cleared). Use this snapshot
                                                  if you do not ramp sample-accurately. */
} TrueHDAtmosMetadata;

/* ------------------------------------------------------------------------------------------ */
/* Decoded blocks                                                                             */
/*
 * SAMPLE RATE: blocks carry the stream's own rate (44.1 to 192 kHz); nothing is resampled and
 * a lossless stream cannot be decoded at a lower rate. Every presentation of a stream has the
 * same rate. Atmos (presentation 3) is 48 kHz in all content tested; should a 96 kHz Atmos
 * stream appear, its blocks report 96000 Hz / 80 frames per access unit and all OAMD timing
 * (frame_offset, ramp_frames) is in frames at that rate. A payload's own timing counts samples at a
 * rate it does not state: TS 103 420 5.3 defines one frame of metadata as 1536 of them. At 48 or
 * 44.1 kHz that is the stream's rate. At a multiple of it the decoder measures the interval
 * between payloads, takes 1536 x rate / interval snapped to 1, 2 or 4 times the base rate, and
 * converts to the stream's frames. A stream's first payload waits for its second, so at those
 * rates the first metadata after create comes up to one payload interval later; reset() keeps
 * what was measured, so a seek within the stream does not wait, and a different stream
 * configuration (major sync) or rate measures again.
 *
 * INPUT TIMELINE: input_access_unit counts every access unit in the bytes pushed since
 * create()/reset(), in stream order, whether or not it was decoded: the ones skipped while
 * waiting for the first major sync, encoder duplicates, and ones that failed to decode all
 * count. input_frame_offset = input_access_unit * access_unit_frames is therefore the number
 * of input frames preceding this block's first frame. To place a block on the source
 * timeline after a seek: time = (pts of the first push after reset) +
 * input_frame_offset / sample_rate. This relies on the first push after create()/reset()
 * starting at an access unit boundary, which demuxed packets (MKV, MP4, M2TS/FFmpeg) always
 * do. Counting is per access unit, not per push: an access unit split across several pushes is
 * counted once, attributed to the push holding its first byte; a trailing incomplete access
 * unit is counted when it completes. The access units before the first major sync are found
 * by following their length fields back from it, so the count is exact; if the first push
 * starts mid-access-unit, that leading fragment (the tail of an access unit that began
 * before it) is not counted and the first whole access unit is number 0. Only when damaged
 * bytes had to be skipped (or more than 4 MiB arrived without a major sync) are the access
 * units in them estimated from the average access unit size; then
 * TRUEHD_ATMOS_BLOCK_INPUT_OFFSET_ESTIMATED is set on this and every later block until
 * reset(). The flag is also set when whole access units failed their checks and were counted
 * by their length fields alone. pts_au_index is exact under the same conditions as
 * input_access_unit.
 *
 * LAYOUT: layout_serial identifies the channel roles: sample rate, presentation, channel
 * count, and each channel's speaker code (and, for presentation 3, element kind, which
 * follows from the speaker code: bed/LFE vs object). It is constant for a stream and across
 * reset() on the same stream; it increments, with TRUEHD_ATMOS_BLOCK_LAYOUT_CHANGED on the
 * first block, only when those roles actually change (a different stream or configuration
 * after a splice or seek, a presentation switch). For presentation 3 the roles come from the
 * major sync and agree with the OAMD bed assignment, so metadata elements always match
 * block.speakers index for index; an OAMD payload stating a different element configuration
 * changes the roles (new serial) in the block where its first update starts.
 */

/*
 * SOURCE POSITION: TrueHD carries its own presentation clock. Every restart header (each
 * substream's, at every major sync) states output_timing, the frame at which that access
 * unit plays, modulo 2^16; each access unit after it plays access_unit_frames later, so a
 * block's output_timing is exact from the first block after create()/reset(). A stream that
 * sends hires_output_timing (one bit per access unit, completed some access units after a
 * major sync) adds the high 16 bits: then output_position is the block's frame in 32 bits and
 * TRUEHD_ATMOS_BLOCK_OUTPUT_POSITION is set. Without it, a container timestamp good to within
 * 2^15 frames picks the 2^16-frame window exactly, so a Matroska millisecond becomes the exact
 * frame. A splice or a different stream restarts the clock at its major sync.
 *
 * input_timing is the access unit header's other time: when its first byte enters the
 * decoder's FIFO in the format's buffer model. It runs ahead of output_timing by a latency
 * that varies with the data rate (measured from one access unit to over 70 ms within one
 * stream), so it does not say where a block plays. The framer holds it to the format's bounds
 * (see the framer) to reject damaged access units whose header parity happens to pass.
 */

/* TrueHDAtmosBlock.flags */
/** First block after create/reset/set_presentation, or after data was dropped to resync. */
#define TRUEHD_ATMOS_BLOCK_DISCONTINUITY 0x01u
/** layout_serial differs from the previous block's (also set on the first block). */
#define TRUEHD_ATMOS_BLOCK_LAYOUT_CHANGED 0x02u
/** .metadata is non-NULL (presentation 3 with OAMD, after the first update since reset). */
#define TRUEHD_ATMOS_BLOCK_HAS_METADATA 0x04u
/** .metadata->update_count > 0. */
#define TRUEHD_ATMOS_BLOCK_METADATA_UPDATED 0x08u
/** input_access_unit includes an estimate (see INPUT TIMELINE); sticky until reset(). */
#define TRUEHD_ATMOS_BLOCK_INPUT_OFFSET_ESTIMATED 0x10u
/** .output_position is known (see SOURCE POSITION). */
#define TRUEHD_ATMOS_BLOCK_OUTPUT_POSITION 0x20u

typedef struct TrueHDAtmosBlock {
    uint32_t sample_rate;         /* Hz: 44100, 48000, 88200, 96000, 176400 or 192000 */
    uint32_t frame_count;         /* frames in each channel plane (= access_unit_frames except a
                                     stream's final, trimmed access unit) */
    uint32_t channel_count;       /* planes in .channels; for presentation 3 = element count */
    uint32_t presentation;        /* presentation decoded, 0..3 */
    uint32_t flags;               /* TRUEHD_ATMOS_BLOCK_* */
    uint32_t access_unit_frames;  /* nominal frames per access unit: 40 at 44.1/48 kHz, 80 at
                                     88.2/96 kHz, 160 at 176.4/192 kHz */
    int64_t pts;                  /* pts given to the push() whose bytes contained this access
                                     unit's first byte, or TRUEHD_ATMOS_NO_PTS (also when more
                                     than 65536 pushes were waiting unconsumed and later ones
                                     with other pts values had to share a record) */
    uint32_t pts_au_index;        /* access units that started earlier within that same push:
                                     block time = pts + pts_au_index * access_unit_frames / rate */
    uint32_t layout_serial;       /* see LAYOUT above */
    uint64_t sample_position;     /* frames output since create()/reset(), before this block */
    uint64_t input_access_unit;   /* see INPUT TIMELINE above */
    uint64_t input_frame_offset;  /* input_access_unit * access_unit_frames */
    const float *const *channels; /* channel_count pointers to frame_count floats, planar,
                                     normalised to [-1, 1): value = pcm24 / 8388608 exactly (a
                                     library built without matrix-interpolation, not the
                                     default, decodes an object presentation that interpolates
                                     approximately, and its values can then exceed that range) */
    const uint8_t *speakers;      /* channel_count TRUEHD_ATMOS_SPEAKER_* codes */
    const TrueHDAtmosMetadata *metadata; /* see TRUEHD_ATMOS_BLOCK_HAS_METADATA, else NULL */
    uint32_t bed_channel_count;   /* see truehd_atmos_decoder_set_bed_companion(), else 0 */
    const float *const *bed_channels; /* bed_channel_count planes of frame_count floats, as
                                     .channels; NULL when bed_channel_count is 0 */
    const uint8_t *bed_speakers;  /* bed_channel_count TRUEHD_ATMOS_SPEAKER_* codes, or NULL */
    uint32_t input_timing;        /* 0..65535: the access unit header's input_timing; see SOURCE
                                     POSITION (a buffer time, not where the block plays) */
    uint32_t output_timing;       /* 0..65535: where this block's first frame plays, in frames at
                                     sample_rate, modulo 2^16; see SOURCE POSITION */
    uint32_t output_position;     /* the same position in full 32 bits, with
                                     TRUEHD_ATMOS_BLOCK_OUTPUT_POSITION; else 0 */
} TrueHDAtmosBlock;

/* ------------------------------------------------------------------------------------------ */
/* Stream information                                                                         */

/* TrueHDAtmosStreamInfo.format */
#define TRUEHD_ATMOS_FORMAT_TRUEHD 0u  /* MLP FBA (Dolby TrueHD) */
#define TRUEHD_ATMOS_FORMAT_MLP 1u     /* MLP FBB (DVD-Audio) */

/* TrueHDAtmosPresentationInfo.type */
#define TRUEHD_ATMOS_PRESENTATION_ABSENT ((int32_t)0)
#define TRUEHD_ATMOS_PRESENTATION_INDEPENDENT ((int32_t)1)
#define TRUEHD_ATMOS_PRESENTATION_DOWNMIX ((int32_t)2) /* encoder downmix of presentation .source */
#define TRUEHD_ATMOS_PRESENTATION_COPY ((int32_t)3)    /* identical to presentation .source */

typedef struct TrueHDAtmosPresentationInfo {
    int32_t type;            /* TRUEHD_ATMOS_PRESENTATION_ABSENT/INDEPENDENT/DOWNMIX/COPY */
    int32_t source;          /* DOWNMIX/COPY: the related presentation; otherwise -1 */
    uint32_t channel_count;  /* 0 when absent */
    uint8_t speakers[TRUEHD_ATMOS_MAX_CHANNELS]; /* channel_count codes, then UNKNOWN; for an
                                                    object presentation: bed speakers, then OBJECT */
} TrueHDAtmosPresentationInfo;

typedef struct TrueHDAtmosStreamInfo {
    uint32_t format;                  /* TRUEHD_ATMOS_FORMAT_* */
    uint32_t sample_rate;
    uint32_t access_unit_frames;
    uint32_t substream_count;
    uint32_t selected_presentation;   /* what pull() decodes, 0..3 */
    uint32_t immersive;               /* 1: presentation 3 (16-channel/Atmos) exists */
    uint32_t has_objects;             /* 1: presentation 3 carries objects (Atmos), per the major sync */
    uint32_t oamd_seen;               /* 1: an OAMD payload was decoded since create()/reset() */
    uint32_t layout_serial;           /* layout_serial of the most recent block (0 before any) */
    TrueHDAtmosPresentationInfo presentations[4];
} TrueHDAtmosStreamInfo;

/* Counters since create() or the last reset(). */
typedef struct TrueHDAtmosStats {
    uint64_t bytes_pushed;
    uint64_t bytes_pending;           /* pushed but not yet consumed */
    uint64_t access_units_decoded;
    uint64_t blocks_output;
    uint64_t frames_output;
    uint64_t access_units_skipped;    /* dropped while waiting for a major sync */
    uint64_t duplicate_access_units;  /* encoder duplicates at splice points, dropped */
    uint64_t decode_errors;           /* access units that failed to parse or decode */
    uint64_t sync_errors;             /* access-unit chain breaks (damage/garbage) resynchronised */
    uint64_t oamd_payloads;           /* OAMD payloads decoded */
    uint64_t oamd_errors;             /* OAMD payloads rejected (audio unaffected) */
    uint64_t metadata_updates;        /* updates delivered in blocks */
    uint64_t oamd_offset_mismatches;  /* OAMD payloads whose EMDF smploffst differs from their own
                                         sample_offset (timed by sample_offset) */
} TrueHDAtmosStats;

/* ------------------------------------------------------------------------------------------ */
/* Functions                                                                                  */

typedef struct TrueHDAtmosDecoder TrueHDAtmosDecoder;

/** TRUEHD_ATMOS_API_VERSION of the compiled library. */
uint32_t truehd_atmos_api_version(void);

/** Static, NUL-terminated library and upstream version description. */
const char *truehd_atmos_version_string(void);

/** Static short name of a TRUEHD_ATMOS_SPEAKER_* code ("L", "Tfl", "LFE", "Obj", "?"). */
const char *truehd_atmos_speaker_name(uint8_t speaker);

/**
 * Diagnostic: raises a Rust panic inside the library and catches it, as every function here
 * does with an unexpected one. Returns TRUEHD_ATMOS_OK if unwinding works in your final link
 * (Rust's panic hook prints one line to stderr). Worth calling once in a test target.
 */
TrueHDAtmosStatus truehd_atmos_selftest(void);

/**
 * Creates a decoder. `presentation` is TRUEHD_ATMOS_PRESENTATION_HIGHEST,
 * _HIGHEST_CHANNEL_BASED or 0..3. Returns NULL for an invalid selector.
 */
TrueHDAtmosDecoder *truehd_atmos_decoder_create(int32_t presentation);

/** Destroys a decoder. NULL is ignored. */
void truehd_atmos_decoder_destroy(TrueHDAtmosDecoder *decoder);

/**
 * Seek support: drops buffered input, pending metadata and all decoder state. Output resumes
 * at the first major sync of data pushed afterwards (TrueHD places one every 8..128 access
 * units, <= 107 ms at 48 kHz); earlier access units are skipped but counted in
 * input_access_unit. The next block has TRUEHD_ATMOS_BLOCK_DISCONTINUITY, and
 * sample_position, input_access_unit and the stats restart at 0. Stream info and
 * layout_serial stay. Also clears a TRUEHD_ATMOS_ERR_PANIC state.
 */
TrueHDAtmosStatus truehd_atmos_decoder_reset(TrueHDAtmosDecoder *decoder);

/**
 * Changes the presentation selector. Buffered input is kept; decoding restarts at the next
 * major sync in it (blocks before that are skipped) with the new presentation.
 */
TrueHDAtmosStatus truehd_atmos_decoder_set_presentation(TrueHDAtmosDecoder *decoder,
                                                        int32_t presentation);

/**
 * Bed companion. When enabled and the presentation decoded is the object presentation (3),
 * every block also carries the stream's highest channel-based presentation (7.1, 5.1 or
 * stereo, whichever the stream has) in .bed_channels / .bed_speakers, decoded from the same
 * access unit, so it is sample-aligned with .channels. Otherwise .bed_channel_count is 0.
 * Off after create(). Takes effect like set_presentation(): decoding restarts at the next
 * major sync. reset() keeps the setting.
 */
TrueHDAtmosStatus truehd_atmos_decoder_set_bed_companion(TrueHDAtmosDecoder *decoder,
                                                         bool enabled);

/**
 * When enabled, the matrix interpolation values a stream carries are read and discarded:
 * every primitive matrix holds the coefficients last transmitted until the next update. The
 * decode is then no longer bit-exact on a stream that interpolates (object presentations
 * typically do). Off after create(). Applies to the access units parsed from the next call on;
 * reset() keeps the setting.
 */
TrueHDAtmosStatus truehd_atmos_decoder_set_ignore_interpolation(TrueHDAtmosDecoder *decoder,
                                                                bool ignore);

/**
 * Appends `size` bytes of TrueHD bitstream. `pts` is an arbitrary caller timestamp (for
 * example AVPacket.pts) reported back on the blocks whose access units start in these bytes;
 * pass TRUEHD_ATMOS_NO_PTS if unknown. The bytes are copied; `data` may be reused on return.
 * size == 0 is a no-op (data may then be NULL).
 */
TrueHDAtmosStatus truehd_atmos_decoder_push(TrueHDAtmosDecoder *decoder, const uint8_t *data,
                                            size_t size, int64_t pts);

/**
 * Decodes the next access unit into *block. Returns TRUEHD_ATMOS_OK with a block,
 * TRUEHD_ATMOS_NEED_MORE_DATA when no complete access unit is buffered (block untouched), or
 * an error. Call repeatedly after each push() until it returns NEED_MORE_DATA; at end of
 * stream that drains everything (no separate flush is needed).
 */
TrueHDAtmosStatus truehd_atmos_decoder_pull(TrueHDAtmosDecoder *decoder, TrueHDAtmosBlock *block);

/**
 * Stream layout from the most recent major sync decoded. TRUEHD_ATMOS_ERR_NOT_READY before
 * the first one since create(); after reset() the previous stream's info is still returned
 * until a major sync in the new data replaces it.
 */
TrueHDAtmosStatus truehd_atmos_decoder_get_stream_info(const TrueHDAtmosDecoder *decoder,
                                                       TrueHDAtmosStreamInfo *info);

/**
 * The trim configurations in force as of the block last pulled, timed like
 * TrueHDAtmosMetadata.warp_mode: those of the latest update started at or before its last frame
 * (every one TRUEHD_ATMOS_TRIM_DEFAULT until a trim element arrives). `trims` receives
 * TRUEHD_ATMOS_TRIM_CONFIGS entries. Per-element trim bypass is TRUEHD_ATMOS_ELEMENT_TRIM_BYPASS.
 */
TrueHDAtmosStatus truehd_atmos_decoder_get_trims(const TrueHDAtmosDecoder *decoder,
                                                 TrueHDAtmosTrim *trims);

TrueHDAtmosStatus truehd_atmos_decoder_get_stats(const TrueHDAtmosDecoder *decoder,
                                                 TrueHDAtmosStats *stats);

/**
 * Message describing the most recent recovered error ("" if none). Valid until the next
 * push/pull/reset/set_presentation/destroy on this handle. Never NULL for a valid handle.
 */
const char *truehd_atmos_decoder_last_error(const TrueHDAtmosDecoder *decoder);

#ifdef __cplusplus
}
#endif

#endif /* TRUEHD_ATMOS_H */
