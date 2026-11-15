// A narrow C interface to whisper.cpp for the server's Rust code, so that
// Rust needn't mirror whisper.h's structs, which change between versions.

#ifndef GRENADINE_WHISPER_SHIM_H
#define GRENADINE_WHISPER_SHIM_H

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct gr_whisper gr_whisper;

// Loads the ggml model at `path`, or returns NULL. whisper.cpp's own
// logging is turned off.
gr_whisper *gr_whisper_load(const char *path);

// Transcribes `n` samples of 16 kHz mono audio in English, with `prompt`
// (may be empty) as the text that came before. `quick` skips the retries
// at higher temperatures that whisper.cpp makes when the text looks
// garbled, for a preview. Returns the text, to be freed with
// gr_whisper_free_text, or NULL on failure. Not thread-safe: one call per
// model at a time.
char *gr_whisper_transcribe(gr_whisper *w, const float *samples, size_t n,
                            const char *prompt, int threads, int quick);

void gr_whisper_free_text(char *text);

void gr_whisper_free(gr_whisper *w);

#ifdef __cplusplus
}
#endif

#endif
