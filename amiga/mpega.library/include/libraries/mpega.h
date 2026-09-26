/*
 * mpega.library interface (MPEG audio decoding), as applications know it
 * from Stéphane Tavenard's mpega.library 2.x: same functions, structures,
 * layouts and constants. Written for PicoZorro's drop-in, which decodes on
 * the card (docs/REGISTERS-MPEG.md).
 */
#ifndef LIBRARIES_MPEGA_H
#define LIBRARIES_MPEGA_H

#include <exec/types.h>
#include <utility/hooks.h>

#define MPEGA_VERSION 2

#define MPEGA_QUALITY_LOW    0
#define MPEGA_QUALITY_MEDIUM 1
#define MPEGA_QUALITY_HIGH   2

/* Bitstream access hook: called with a2 = the handle OPEN returned (NULL
 * for OPEN itself), a1 = an MPEGA_ACCESS. OPEN returns the handle (NULL =
 * failure) and may set stream_size (0 = unknown); READ returns the bytes
 * read (0 = end); SEEK and CLOSE return 0 when fine. */
#define MPEGA_BSFUNC_OPEN  0
#define MPEGA_BSFUNC_CLOSE 1
#define MPEGA_BSFUNC_READ  2
#define MPEGA_BSFUNC_SEEK  3

typedef struct {
    LONG func;
    union {
        struct { char *stream_name; LONG buffer_size; LONG stream_size; } open;
        struct { void *buffer; LONG num_bytes; } read;
        struct { LONG abs_byte_seek_pos; } seek;
    } data;
} MPEGA_ACCESS;

typedef struct {
    WORD freq_div;     /* 1, 2 or 4; 0 = the largest rate not above freq_max */
    WORD quality;      /* MPEGA_QUALITY_xxx */
    LONG freq_max;
} MPEGA_OUTPUT;

typedef struct {
    WORD force_mono;       /* 1: decode a stereo stream to mono */
    MPEGA_OUTPUT mono;     /* used for mono streams */
    MPEGA_OUTPUT stereo;   /* used for stereo streams */
} MPEGA_LAYER;

typedef struct {
    struct Hook *bs_access;   /* NULL: the library opens the named file */
    MPEGA_LAYER layer_1_2;
    MPEGA_LAYER layer_3;
    WORD check_mpeg;
    LONG stream_buffer_size;  /* bytes, multiple of 4; 0 = default */
} MPEGA_CTRL;

#define MPEGA_MODE_STEREO   0
#define MPEGA_MODE_J_STEREO 1
#define MPEGA_MODE_DUAL     2
#define MPEGA_MODE_MONO     3

typedef struct {
    /* read only */
    WORD norm;           /* 1: MPEG-1, 2: MPEG-2 and 2.5 */
    WORD layer;          /* 1..3 */
    WORD mode;           /* MPEGA_MODE_xxx */
    WORD bitrate;        /* kbps */
    LONG frequency;      /* Hz */
    WORD channels;       /* 1 or 2 */
    ULONG ms_duration;
    WORD private_bit;
    WORD copyright;
    WORD original;
    /* the output, as MPEGA_CTRL asked */
    WORD dec_channels;
    WORD dec_quality;
    LONG dec_frequency;
    /* private */
    void *handle;
} MPEGA_STREAM;

#define MPEGA_MAX_CHANNELS 2
#define MPEGA_PCM_SIZE     1152

#define MPEGA_ERR_NONE     0
#define MPEGA_ERR_BASE     0
#define MPEGA_ERR_EOF      (MPEGA_ERR_BASE - 1)
#define MPEGA_ERR_BADFRAME (MPEGA_ERR_BASE - 2)
#define MPEGA_ERR_MEM      (MPEGA_ERR_BASE - 3)
#define MPEGA_ERR_NO_SYNC  (MPEGA_ERR_BASE - 4)
#define MPEGA_ERR_BADVALUE (MPEGA_ERR_BASE - 5)

#endif
