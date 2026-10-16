// Flat C shim over llama.cpp / ggml / mtmd.
//
// llama_model_params and llama_context_params are passed BY VALUE in the
// upstream C API and are large, version-dependent structs. Keeping them on the
// C++ side means the Rust FFI only sees pointers and scalars, so a pinned
// llama.cpp bump changes this file, not the Rust declarations.

#include "llama.h"
#include "ggml-backend.h"
#include "ggml.h"
#include "mtmd.h"
#include "mtmd-helper.h"

#include <cstdint>
#include <cstring>
#include <string>
#include <vector>

extern "C" {

// ---- backend registry ------------------------------------------------------

// Loads every backend DLL found next to the executable (GGML_BACKEND_DL).
void wse_backend_load_all(void) {
    ggml_backend_load_all();
}

// Loads backend DLLs from an explicit directory.
void wse_backend_load_all_from_path(const char * dir) {
    ggml_backend_load_all_from_path(dir);
}

size_t wse_dev_count(void) {
    return ggml_backend_dev_count();
}

// Returns the device at index, or NULL.
ggml_backend_dev_t wse_dev_get(size_t index) {
    return ggml_backend_dev_get(index);
}

// Writes the registry name ("CUDA"/"Vulkan"/"CPU"/...) into buf (NUL-terminated).
size_t wse_dev_backend_name(ggml_backend_dev_t dev, char * buf, size_t cap) {
    if (!dev || !buf || cap == 0) {
        return 0;
    }
    const char * name = ggml_backend_reg_name(ggml_backend_dev_backend_reg(dev));
    if (!name) {
        name = "";
    }
    size_t n = std::strlen(name);
    if (n >= cap) {
        n = cap - 1;
    }
    std::memcpy(buf, name, n);
    buf[n] = '\0';
    return n;
}

// 0 = CPU, 1 = GPU, 2 = IGPU, 3 = ACCEL, 4 = META, -1 = invalid
int wse_dev_type(ggml_backend_dev_t dev) {
    if (!dev) {
        return -1;
    }
    return (int) ggml_backend_dev_type(dev);
}

void wse_dev_memory(ggml_backend_dev_t dev, size_t * free_bytes, size_t * total_bytes) {
    ggml_backend_dev_memory(dev, free_bytes, total_bytes);
}

// Writes the device name into buf (NUL-terminated); returns bytes written.
size_t wse_dev_name(ggml_backend_dev_t dev, char * buf, size_t cap) {
    if (!dev || !buf || cap == 0) {
        return 0;
    }
    const char * name = ggml_backend_dev_name(dev);
    if (!name) {
        name = "";
    }
    size_t n = std::strlen(name);
    if (n >= cap) {
        n = cap - 1;
    }
    std::memcpy(buf, name, n);
    buf[n] = '\0';
    return n;
}

// ---- logging ----------------------------------------------------------------

// Installs a callback. Pass NULL to restore the default stderr output.
void wse_log_set(ggml_log_callback cb, void * user_data) {
    llama_log_set(cb, user_data);
}

// ---- model ------------------------------------------------------------------

// Loads a GGUF. backend_dev may be NULL (= all devices). n_gpu_layers < 0 means
// all layers; 0 forces CPU. Returns NULL on failure.
struct llama_model * wse_model_load(const char * path, ggml_backend_dev_t backend_dev, int32_t n_gpu_layers) {
    llama_model_params params = llama_model_default_params();
    params.n_gpu_layers = n_gpu_layers;
    ggml_backend_dev_t devices[2] = { backend_dev, nullptr };
    if (backend_dev) {
        params.devices = devices;
    }
    return llama_model_load_from_file(path, params);
}

void wse_model_free(struct llama_model * model) {
    llama_model_free(model);
}

// Pooled OUTPUT embedding dimension. Not llama_model_n_embd, which is the input
// width: EmbeddingGemma 2 has input 512 but output 768 (embedding_length_out).
int32_t wse_model_n_embd(const struct llama_model * model) {
    return llama_model_n_embd_out(model);
}

// ---- context ----------------------------------------------------------------

// Maximum sequences per decode. The context must be created with this capacity:
// llama_batch rejects any seq_id >= n_seq_max (default 1), which made every
// multi-text batch fail with -1. Callers chunk to this size.
#define WSE_N_SEQ_MAX 32

// Creates an embedding-mode context. The pooling type is taken from the model
// (pooling_type = UNSPECIFIED).
struct llama_context * wse_context_new(struct llama_model * model, uint32_t n_ctx, uint32_t n_batch, int32_t n_threads) {
    llama_context_params params = llama_context_default_params();
    params.n_ctx = n_ctx;
    params.n_batch = n_batch;
    params.n_ubatch = n_batch;
    params.n_seq_max = WSE_N_SEQ_MAX;
    params.n_threads = n_threads;
    params.n_threads_batch = n_threads;
    params.embeddings = true;
    params.no_perf = true;
    return llama_init_from_model(model, params);
}

// Sequences per decode the context supports (chunk size for callers).
int32_t wse_context_max_seqs(void) {
    return WSE_N_SEQ_MAX;
}

void wse_context_free(struct llama_context * ctx) {
    llama_free(ctx);
}

int32_t wse_context_pooling_type(const struct llama_context * ctx) {
    return (int32_t) llama_pooling_type(ctx);
}

// ---- tokenize / embed --------------------------------------------------------

// Tokenizes text with special tokens (BOS/EOS as the model defines). Returns the
// token count, or a negative value holding the required size (-n) when cap is
// too small, matching llama_tokenize semantics.
int32_t wse_tokenize(const struct llama_model * model, const char * text, int32_t text_len, int32_t * out, int32_t cap) {
    const struct llama_vocab * vocab = llama_model_get_vocab(model);
    return llama_tokenize(vocab, text, text_len, (llama_token *) out, cap, true, true);
}

// Runs one embedding pass over a batch of sequences and copies each pooled
// sequence embedding into out (row-major, n_seqs x n_embd).
//
// tokens: all sequences concatenated; seq_lens[i] = token count of sequence i.
// Returns 0 on success, non-zero llama_decode code on failure, -1 on bad args.
int32_t wse_embed_batch(struct llama_context * ctx, const int32_t * tokens, const int32_t * seq_lens, int32_t n_seqs, int32_t n_embd, float * out) {
    if (!ctx || !tokens || !seq_lens || n_seqs <= 0 || n_seqs > WSE_N_SEQ_MAX || n_embd <= 0 || !out) {
        return -1;
    }
    int32_t n_total = 0;
    for (int32_t s = 0; s < n_seqs; ++s) {
        if (seq_lens[s] <= 0) {
            return -1;
        }
        n_total += seq_lens[s];
    }
    const uint32_t max_batch = llama_n_batch(ctx);
    const uint32_t max_ubatch = llama_n_ubatch(ctx);
    if ((max_batch > 0 && (uint32_t) n_total > max_batch) ||
        (max_ubatch > 0 && (uint32_t) n_total > max_ubatch)) {
        return -1;
    }

    llama_memory_clear(llama_get_memory(ctx), true);

    llama_batch batch = llama_batch_init(n_total, 0, n_seqs);
    int32_t idx = 0;
    for (int32_t s = 0; s < n_seqs; ++s) {
        for (int32_t t = 0; t < seq_lens[s]; ++t) {
            batch.token[idx] = tokens[idx];
            batch.pos[idx] = t;
            batch.n_seq_id[idx] = 1;
            batch.seq_id[idx][0] = s;
            batch.logits[idx] = 1;
            ++idx;
        }
    }
    batch.n_tokens = n_total;

    int32_t rc = llama_decode(ctx, batch);
    if (rc == 0) {
        for (int32_t s = 0; s < n_seqs; ++s) {
            float * e = llama_get_embeddings_seq(ctx, s);
            if (!e) {
                rc = -2;
                break;
            }
            std::memcpy(out + (size_t) s * n_embd, e, sizeof(float) * n_embd);
        }
    }
    llama_batch_free(batch);
    return rc;
}

// ---- multimodal (mtmd) -------------------------------------------------------

// Opens the mmproj. use_gpu=false forces the projector onto CPU. Returns NULL on
// failure. Vision/audio entry points are stubs until Phase 4 wires them.
mtmd_context * wse_mtmd_init(const char * mmproj_path, const struct llama_model * text_model, bool use_gpu, int32_t n_threads) {
    mtmd_context_params p = mtmd_context_params_default();
    p.use_gpu = use_gpu;
    p.n_threads = n_threads;
    p.warmup = false;
    return mtmd_init_from_file(mmproj_path, text_model, p);
}

void wse_mtmd_free(mtmd_context * ctx) {
    mtmd_free(ctx);
}

int32_t wse_mtmd_supports_vision(const mtmd_context * ctx) {
    return mtmd_support_vision(ctx) ? 1 : 0;
}

int32_t wse_mtmd_supports_audio(const mtmd_context * ctx) {
    return mtmd_support_audio(ctx) ? 1 : 0;
}

// Encodes an RGB8 image (`width * height * 3` bytes) through the mtmd vision
// projector + text model context and copies the pooled `n_embd` vector into `out`.
// Returns 0 on success, non-zero on failure.
int32_t wse_mtmd_embed_image(
    mtmd_context * mctx,
    struct llama_context * lctx,
    const unsigned char * rgb,
    uint32_t width,
    uint32_t height,
    int32_t n_batch,
    int32_t n_embd,
    float * out
) {
    if (!mctx || !lctx || !rgb || width == 0 || height == 0 || n_embd <= 0 || !out) {
        return -1;
    }
    if (!mtmd_support_vision(mctx)) {
        return -2;
    }
    mtmd_bitmap * bmp = mtmd_bitmap_init(width, height, rgb);
    if (!bmp) {
        return -3;
    }
    const char * marker = mtmd_default_marker();
    if (!marker) {
        marker = "<__media__>";
    }
    mtmd_input_text inp_text;
    inp_text.text = marker;
    inp_text.text_len = std::strlen(marker);
    inp_text.add_special = true;
    inp_text.parse_special = true;

    mtmd_input_chunks * chunks = mtmd_input_chunks_init();
    const mtmd_bitmap * bitmaps[1] = { bmp };
    int32_t tok_rc = mtmd_tokenize(mctx, chunks, &inp_text, bitmaps, 1);
    if (tok_rc != 0) {
        mtmd_input_chunks_free(chunks);
        mtmd_bitmap_free(bmp);
        return -4;
    }

    llama_memory_clear(llama_get_memory(lctx), true);
    llama_pos new_n_past = 0;
    int32_t eval_rc = mtmd_helper_eval_chunks(
        mctx,
        lctx,
        chunks,
        0,
        0,
        n_batch > 0 ? n_batch : 512,
        true,
        &new_n_past
    );
    int32_t rc = eval_rc;
    if (rc == 0) {
        float * e = llama_get_embeddings_seq(lctx, 0);
        if (!e) {
            e = llama_get_embeddings_ith(lctx, -1);
        }
        if (!e) {
            rc = -5;
        } else {
            std::memcpy(out, e, sizeof(float) * (size_t) n_embd);
        }
    }

    mtmd_input_chunks_free(chunks);
    mtmd_bitmap_free(bmp);
    return rc;
}

} // extern "C"
