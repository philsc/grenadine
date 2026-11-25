#include "grenadine/server/whisper/shim.h"

#include <cstdlib>
#include <cstring>
#include <string>

#include "whisper.h"

struct gr_whisper {
  whisper_context *ctx;
};

using error_callback = void (*)(const char *);

static void log_errors(enum ggml_log_level level, const char *text,
                       void *on_error) {
  if (level == GGML_LOG_LEVEL_ERROR) {
    reinterpret_cast<error_callback>(on_error)(text);
  }
}

gr_whisper *gr_whisper_load(const char *path,
                            void (*on_error)(const char *message)) {
  whisper_log_set(log_errors, reinterpret_cast<void *>(on_error));
  whisper_context_params params = whisper_context_default_params();
  params.use_gpu = false;
  params.flash_attn = true;
  whisper_context *ctx = whisper_init_from_file_with_params(path, params);
  if (ctx == nullptr) {
    return nullptr;
  }
  return new gr_whisper{ctx};
}

char *gr_whisper_transcribe(gr_whisper *w, const float *samples, size_t n,
                            const char *prompt, int threads, int quick) {
  whisper_full_params params =
      whisper_full_default_params(WHISPER_SAMPLING_GREEDY);
  params.n_threads = threads;
  params.language = "en";
  params.translate = false;
  params.no_context = true;
  params.no_timestamps = true;
  params.single_segment = true;
  params.print_special = false;
  params.print_progress = false;
  params.print_realtime = false;
  params.print_timestamps = false;
  params.suppress_nst = true;
  if (quick) {
    params.temperature_inc = 0.0f;
  }
  // The encoder looks at 30 s of audio, 50 frames a second, unless told
  // to look at less. Phrases are much shorter, and encoding just them
  // (with some margin) is several times faster.
  const int frames = static_cast<int>(n / 320) + 128;
  params.audio_ctx = frames < 1500 ? frames : 0;
  if (prompt != nullptr && prompt[0] != '\0') {
    params.initial_prompt = prompt;
  }
  if (whisper_full(w->ctx, params, samples, static_cast<int>(n)) != 0) {
    return nullptr;
  }
  std::string text;
  const int segments = whisper_full_n_segments(w->ctx);
  for (int i = 0; i < segments; ++i) {
    text += whisper_full_get_segment_text(w->ctx, i);
  }
  return strdup(text.c_str());
}

void gr_whisper_free_text(char *text) { free(text); }

void gr_whisper_free(gr_whisper *w) {
  if (w != nullptr) {
    whisper_free(w->ctx);
    delete w;
  }
}
