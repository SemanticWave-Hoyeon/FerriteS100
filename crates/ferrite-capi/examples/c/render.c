/*
 * render.c - open S-100 datasets, render them offscreen and pick a feature
 * through the ferrite C ABI.
 *
 *   cc -I crates/ferrite-capi/include examples/c/render.c -L <target>/debug -lferrite -o render
 *   ./render CATALOGUES DATASET_FOLDER OUT.ppm [LAT LON ZOOM]
 *
 * Writes the image as a binary PPM (RGB) and prints the dataset count, a
 * pixel checksum and the features under the image centre.
 */
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "ferrite.h"

#define WIDTH 800
#define HEIGHT 600

static int fail(const char *what, FerriteEngine *engine) {
    fprintf(stderr, "%s failed: %s\n", what, ferrite_last_error());
    ferrite_engine_destroy(engine);
    return 1;
}

int main(int argc, char **argv) {
    if (argc != 4 && argc != 7) {
        fprintf(stderr, "usage: %s CATALOGUES DATASET_FOLDER OUT.ppm [LAT LON ZOOM]\n", argv[0]);
        return 2;
    }
    printf("ferrite %s\n", ferrite_version());

    FerriteEngine *engine = ferrite_engine_create(argv[1], NULL, NULL);
    if (engine == NULL) {
        return fail("ferrite_engine_create", NULL);
    }
    if (ferrite_engine_open_folder(engine, argv[2]) != FERRITE_OK) {
        return fail("ferrite_engine_open_folder", engine);
    }
    long long datasets = (long long)ferrite_engine_dataset_count(engine);
    printf("datasets: %lld\n", datasets);
    if (datasets <= 0) {
        return fail("no datasets opened", engine);
    }

    FerriteCamera camera = {0};
    const FerriteCamera *view = NULL;
    if (argc == 7) {
        camera.has_center = 1;
        camera.latitude = atof(argv[4]);
        camera.longitude = atof(argv[5]);
        camera.zoom = atof(argv[6]);
        view = &camera;
    }
    if (ferrite_engine_prepare(engine, WIDTH, HEIGHT, 1.0, view) != FERRITE_OK) {
        return fail("ferrite_engine_prepare", engine);
    }

    size_t length = (size_t)WIDTH * HEIGHT * 4;
    unsigned char *rgba = malloc(length);
    if (rgba == NULL) {
        return fail("malloc", engine);
    }
    /* A too small buffer is refused, not overrun. */
    if (ferrite_engine_render_rgba(engine, rgba, length - 1) != FERRITE_BUFFER_TOO_SMALL) {
        free(rgba);
        return fail("short buffer check", engine);
    }
    if (ferrite_engine_render_rgba(engine, rgba, length) != FERRITE_OK) {
        free(rgba);
        return fail("ferrite_engine_render_rgba", engine);
    }

    unsigned long long checksum = 0;
    size_t distinct = 0;
    for (size_t i = 0; i < length; i += 4) {
        checksum = checksum * 31 + rgba[i] + (rgba[i + 1] << 8) + (rgba[i + 2] << 16);
        if (i >= 4 && memcmp(rgba + i, rgba + i - 4, 3) != 0) {
            distinct++;
        }
    }
    printf("image: %dx%d checksum %016llx colour changes %zu\n", WIDTH, HEIGHT, checksum, distinct);

    FILE *out = fopen(argv[3], "wb");
    if (out == NULL) {
        free(rgba);
        return fail("fopen", engine);
    }
    fprintf(out, "P6\n%d %d\n255\n", WIDTH, HEIGHT);
    for (size_t i = 0; i < length; i += 4) {
        fwrite(rgba + i, 1, 3, out);
    }
    fclose(out);
    free(rgba);

    char *hits = ferrite_engine_pick_json(engine, WIDTH / 2.0, HEIGHT / 2.0, 40.0f);
    if (hits == NULL) {
        return fail("ferrite_engine_pick_json", engine);
    }
    printf("pick: %.200s%s\n", hits, strlen(hits) > 200 ? "..." : "");
    ferrite_string_free(hits);

    char *report = ferrite_engine_report_json(engine);
    if (report == NULL) {
        return fail("ferrite_engine_report_json", engine);
    }
    printf("report: %zu bytes of JSON\n", strlen(report));
    ferrite_string_free(report);

    /* Errors are reported, not crashed on. */
    if (ferrite_engine_set_palette(engine, "NoSuchPalette") != FERRITE_ERROR) {
        return fail("palette check", engine);
    }
    printf("expected error: %s\n", ferrite_last_error());

    ferrite_engine_destroy(engine);
    printf("ok\n");
    return 0;
}
