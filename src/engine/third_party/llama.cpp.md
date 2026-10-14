# llama.cpp (vendored pin)

The llama.cpp source tree is **not** committed. `crates/ws-embed/build.rs`
fetches the pinned commit on first build into `target/ws-embed-llama/` (shallow,
single commit). Override with `LLAMA_CPP_DIR=<checkout of the same commit>`.

| Field        | Value                                                        |
|--------------|--------------------------------------------------------------|
| Repository   | https://github.com/ggml-org/llama.cpp                        |
| Pinned rev   | `4fbc76dec51d0add466f0210855c0596589b60d4`                   |
| Committed    | 2026-10-06, `model: support embeddinggemma2 (text+vision+audio) (#30054)` |
| Licence      | MIT (llama.cpp / ggml)                                       |

## Why a commit, not a release tag

The plan asked for a recent stable release tag. None works for this model:

- `v0.6.0` (latest release, 2026-10-05) has **no** `gemma-embedding2`
  architecture. Loading `embeddinggemma-2-Q8_0.gguf` fails with
  `unknown model architecture: 'gemma-embedding2'`.
- The support landed in `4fbc76dec`, 23 commits after `v0.6.0`, and is not in any
  release tag yet.

So the pin is that commit. Move to a release tag when one contains the
architecture. Re-verify the reference vectors on any bump (see below).

## Build configuration

Set by `build.rs` (Release, MSVC x64, Visual Studio 17 2022):

- `BUILD_SHARED_LIBS=ON`, `GGML_BACKEND_DL=ON`: backends are separate DLLs.
- `GGML_CPU_ALL_VARIANTS=ON`: CPU backend built as several ISA variants, the best
  one chosen at runtime. The CPU backend always ships.
- `GGML_CUDA=ON` only when the CUDA toolkit is found (`CUDA_PATH` or the default
  install dir). `WS_EMBED_NO_CUDA=1` forces OFF.
- `GGML_VULKAN=ON` only when `VULKAN_SDK` is set. `WS_EMBED_NO_VULKAN=1` forces OFF.
- `LLAMA_BUILD_MTMD=ON`; tools, tests, server, UI, examples OFF; OpenSSL OFF;
  OpenMP OFF.
- Each backend combination gets its own build dir (keyed by CUDA/Vulkan state),
  so a CPU-only build never reuses a CUDA configure.

## Patch notes

**No upstream source patches.** The integration is a flat C shim
(`crates/ws-embed/shim/ws_embed_shim.cpp`) compiled against the unmodified
headers. It keeps the large by-value `llama_model_params` /
`llama_context_params` structs on the C++ side.

Shim-level workarounds for this revision:

- `n_seq_max` defaults to **1** and `llama_batch` rejects `seq_id >= n_seq_max`
  (`src/llama-batch.cpp`). The shim sets it to 32 (`WSE_N_SEQ_MAX`) and
  `ws-embed` chunks batches to that size. Without this every multi-text batch
  failed with `llama_decode` returning -1.
- The embedding dimension comes from `llama_model_n_embd_out`, not
  `llama_model_n_embd`. The latter is the input width (512 for EmbeddingGemma 2);
  the pooled output is 768.

## Reference vectors

`tests/fixtures/embeddinggemma2-reference.json` holds 10 pinned inputs and the
raw pooled vectors produced by this revision's `llama-embedding` (built from the
same tree, `--pooling mean --embd-normalize -1`). The `reference` test requires
every runtime vector to match within cosine drift `1e-3`.

- Model: `ggml-org/embeddinggemma-2-GGUF` `embeddinggemma-2-Q8_0.gguf`
  (309,855,456 bytes). Not committed. Run with `WS_EMBED_TEST_MODEL`.
- Measured worst drift at this pin: **1.32e-4** (CPU).

Caveat: the reference comes from the same llama.cpp codepath, so it verifies the
shim matches upstream. It does not independently validate EmbeddingGemma against
Google's reference implementation.

## Upgrade procedure

1. Change `LLAMA_CPP_REV` and `LLAMA_CPP_LABEL` in `build.rs`; update this file.
2. Rebuild and run the model-gated tests with `WS_EMBED_TEST_MODEL` set.
3. If pooling or values drift, regenerate the reference fixture from the new
   revision and record the reason here. Do not loosen the tolerance.
