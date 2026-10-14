# llama.cpp (vendored pin)

The llama.cpp source tree is **not** committed. `crates/ws-embed/build.rs`
fetches it on first build and caches it under `target/ws-embed-llama/`.

| Field        | Value                                                    |
|--------------|----------------------------------------------------------|
| Repository   | https://github.com/ggml-org/llama.cpp                    |
| Pinned tag   | `v0.6.0`                                                 |
| Commit       | `d81235049384534c167caea52b85a694f6103d14`               |
| ggml version | 0.26.0 (bundled in the tag)                              |
| Licence      | MIT (llama.cpp / ggml)                                   |

Override the source with `LLAMA_CPP_DIR=<path to a checkout of the same tag>`.

## Build configuration

Set by `build.rs` (Release, MSVC x64, Visual Studio 17 2022):

- `BUILD_SHARED_LIBS=ON`, `GGML_BACKEND_DL=ON`: backends are separate DLLs.
- `GGML_CPU_ALL_VARIANTS=ON`: CPU backend is built as several ISA variants and
  the best one is chosen at runtime. The CPU backend always ships.
- `GGML_CUDA=ON` only when the CUDA toolkit is found (`CUDA_PATH` or the default
  install dir). Otherwise OFF.
- `GGML_VULKAN=ON` only when `VULKAN_SDK` is set. Otherwise OFF (skipped).
- `LLAMA_BUILD_MTMD=ON` (multimodal projector library), tools/tests/server/UI OFF,
  OpenSSL OFF, OpenMP OFF (no extra runtime DLL).

## Patch notes

**No upstream source patches.** The integration is a flat C shim
(`crates/ws-embed/shim/ws_embed_shim.cpp`) compiled against the unmodified
headers. It keeps the large by-value `llama_model_params` /
`llama_context_params` structs on the C++ side.

Shim-level workarounds for behaviour in this tag:

- `llama_context_params.n_seq_max` defaults to **1**, and `llama_batch` rejects
  any `seq_id >= n_seq_max` (`src/llama-batch.cpp:175`). The shim sets it to 32
  (`WSE_N_SEQ_MAX`) and `ws-embed` chunks batches to that size. Without this every
  multi-text batch failed with `llama_decode` returning -1.

## Upgrade procedure

A pin bump is a deliberate change: update the tag and commit above, rebuild, and
re-run the reference-vector tests (`WS_EMBED_TEST_MODEL` set) before merging.
Pooling behaviour can change across llama.cpp releases, which is why the
reference vectors are the gate, not the tag alone.
