# Performance and memory (measured 2026-11-02)

Machine: AMD Ryzen 9 3900X (12 cores), 48 GB RAM, NVIDIA RTX 5080 16 GB, Windows 11, release build.
Data: USA Wind Waker HD dump, 1007 content files.

| Stage | Work | Time | Notes |
|---|---|---|---|
| Scan | 1007 files, 14 572 textures, gate on every texture | 3.6 s | parallel over files |
| Extract | 5 491 unique masters + thumbnails | 32 s | content-addressed; a rerun skips existing files |
| Upscale | 5 208 tasks (Real-ESRGAN x4plus-anime for ~3 300 colour textures, filters for the rest) | ~15 min | 32 textures per ncnn run; 0 failures, 0 consistency rejections |
| Model comparison | 8 textures × 18 backends | 62 s | `compare` command |
| Export (cold) | 12 347 textures, 740 archives, 14 packages | 15 min (4 threads, no encode cache) | BC encode at ISPC "slow", mip chains, tiling, Yaz0, full validation |
| Export (12 threads) | 12 358 textures, 740 archives | 4.5 min | |
| Re-export with the memory-safe defaults (cached encodes) | 5 531 textures, 670 archives, 5 packages | 78 s | 0 errors |

Export uses half the logical CPUs (max 12) and caches encoded surfaces, so shared textures are encoded once
and a re-export after review changes only re-encodes what changed.

## Memory

| Process | Working set |
|---|---|
| GUI (debug build) after browsing ~2 000 thumbnails | ~230 MB (thumbnail cache bounded to 1 500 textures) |
| Headless upscale | ~90 MB plus the ncnn tool (one batch of up to 32 textures) |
| Headless export | ~0.5 GB with 4 threads (one archive per thread, 4K surfaces + mip chains) |

The whole texture set is never loaded at once: upscaling streams batches through the external tool, export
works archive by archive.

## Output size

| | Original | Unlimited ×4 (does not run in game) | Memory-safe default (runs) |
|---|---|---|---|
| Archives rebuilt | — | 740 | 670 |
| Textures replaced | — | 12 358 | 5 531 |
| Rebuilt archives on disc (Yaz0) | 1 369 / 1 157 MiB | 6 562 MiB | 2 066 MiB |
| Texture data in those archives, all copies | — | +20 324 MiB | +2 436 MiB |

BC data compresses poorly with Yaz0, so packages are large. The always-loaded packs (`Common/Pack/`) are
excluded by default because the game unpacks their inner archives into fixed-size heaps.

## VR considerations

Each upscaled texture keeps the original's smallest mip size, so distant surfaces sample the same small mips
as before; only close-up detail uses the large levels. Guest-memory use grows with the per-archive budget; if
an area fails to load in game, lower *max extra texture memory per archive* in Settings (or the scale) and
re-export — encoded surfaces are cached, so this is quick.
