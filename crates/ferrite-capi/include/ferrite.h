/*
 * ferrite.h - C ABI of the FerriteS100 S-100 chart engine (ferrite-capi).
 *
 * Link with libferrite (cdylib: libferrite.dylib / libferrite.so /
 * ferrite.dll, or the static libferrite.a with the platform's GPU
 * frameworks). Hand-written; crates/ferrite-capi/tests/header.rs checks that
 * every function declared here is exported by the library and vice versa.
 *
 * Conventions
 *   - FerriteEngine is an opaque handle: ferrite_engine_create /
 *     ferrite_engine_destroy. A handle is used by one thread at a time.
 *   - Functions returning int32_t return FERRITE_OK (0) or a negative status;
 *     ferrite_last_error() describes the last failure on the calling thread.
 *   - char * results are UTF-8 JSON owned by the caller: release them with
 *     ferrite_string_free. They are NULL on failure.
 *   - No panic crosses the ABI (reported as FERRITE_PANIC).
 */
#ifndef FERRITE_H
#define FERRITE_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define FERRITE_OK 0
/* The operation failed; see ferrite_last_error(). */
#define FERRITE_ERROR (-1)
/* A null handle or pointer, invalid UTF-8 or an out-of-range value. */
#define FERRITE_INVALID_ARGUMENT (-2)
/* No scene was prepared (call ferrite_engine_prepare first). */
#define FERRITE_NO_SCENE (-3)
/* The output buffer is smaller than width * height * 4 bytes. */
#define FERRITE_BUFFER_TOO_SMALL (-4)
/* A panic inside the library was caught. */
#define FERRITE_PANIC (-5)

typedef struct FerriteEngine FerriteEngine;

/* Camera of ferrite_engine_prepare. */
typedef struct FerriteCamera {
    /* Non-zero: centre the view on latitude/longitude (degrees). */
    int32_t has_center;
    double latitude;
    double longitude;
    /* Zoom over the all-data view (> 0), or 0 to use scale_denominator. */
    double zoom;
    /* Display scale 1:N when zoom is 0 (0 with zoom 0: all data). */
    double scale_denominator;
} FerriteCamera;

/* Library version (static string). */
const char *ferrite_version(void);

/* Message of the last failure on this thread ("" after a success); valid
 * until the next library call on this thread. */
const char *ferrite_last_error(void);

/* Release a string returned by the library (NULL is ignored). */
void ferrite_string_free(char *string);

/* Create an engine with the catalogue folder `catalogues` (FC/S-101,
 * PC/S-101, optional S-124/S-129). `inventory` (nullable): installed S-101
 * catalogue inventory for datasets of other editions. `cache_dir`
 * (nullable): per-cell portrayal caches and precomputed payloads (NULL: no
 * disk cache). Returns NULL on failure. */
FerriteEngine *ferrite_engine_create(const char *catalogues,
                                     const char *inventory,
                                     const char *cache_dir);

/* Destroy an engine (NULL is ignored). */
void ferrite_engine_destroy(FerriteEngine *engine);

/* Open every dataset under a folder (exchange sets included). Rejected
 * datasets are listed by ferrite_engine_report_json, not a failure. */
int32_t ferrite_engine_open_folder(FerriteEngine *engine, const char *path);

/* Open dataset files; an S-101 base and its updates belong in one call. */
int32_t ferrite_engine_open_files(FerriteEngine *engine,
                                  const char *const *paths,
                                  size_t count);

/* Open datasets held in memory: names[i] (file name, e.g.
 * "101FR00369400.000") identifies data[i] of lengths[i] bytes. The bytes
 * are copied. No signature verification. A root-relative name ("/xs/...")
 * is an absolute memory key on every platform; on Windows it is placed on
 * the virtual drive C: (reported as C:\xs\...). */
int32_t ferrite_engine_open_bytes(FerriteEngine *engine,
                                  const char *const *names,
                                  const uint8_t *const *data,
                                  const size_t *lengths,
                                  size_t count);

/* Number of opened datasets, or a negative status. */
int64_t ferrite_engine_dataset_count(FerriteEngine *engine);

/* Colour profile of the next prepared scene: "Day", "Dusk" or "Night". */
int32_t ferrite_engine_set_palette(FerriteEngine *engine, const char *palette);

/* Non-zero: portray S-101 cells from precomputed payloads in the cache
 * directory instead of running the Lua rules. */
int32_t ferrite_engine_set_precomputed(FerriteEngine *engine, int32_t precomputed);

/* Portray the opened datasets for a width x height pixel view at `density`
 * physical pixels per logical pixel. camera NULL: all data. */
int32_t ferrite_engine_prepare(FerriteEngine *engine,
                               uint32_t width,
                               uint32_t height,
                               double density,
                               const FerriteCamera *camera);

/* Render the prepared scene offscreen (GPU) into rgba: width * height * 4
 * bytes, rows top to bottom, RGBA8. */
int32_t ferrite_engine_render_rgba(FerriteEngine *engine, uint8_t *rgba, size_t length);

/* Features under pixel (x, y) of the prepared scene within radius pixels,
 * best first: JSON array of feature reports. */
char *ferrite_engine_pick_json(FerriteEngine *engine, double x, double y, float radius);

/* Opened datasets with feature counts, open issues and notices, and the
 * exchange-set checks, as a JSON object. */
char *ferrite_engine_report_json(FerriteEngine *engine);

#ifdef __cplusplus
}
#endif

#endif /* FERRITE_H */
