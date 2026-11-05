# ZeldaWWHD Texture Studio

A lightweight native desktop app (Rust + egui) that takes your own extracted **The Legend of Zelda: The Wind
Waker HD** (Wii U) disc dump, extracts every texture, classifies it, upscales it with conventional or AI
models (Real-ESRGAN, waifu2x, Real-CUGAN, RealSR), re-encodes it in the texture's original GX2 format with a
proper mip chain, and exports the result for the ZeldaWWHD recomp's mod manager in one of two ways: **runtime texture
packs** (BC7/BC5/BC4 DDS files the recomp binds at draw time, ×2 to ×32) or **rebuilt-archive content packages**.
The disc dump is never modified.

No game data is part of this repository, and none is written into it.

## Status

| Area | State |
|---|---|
| Disc scan (Yaz0, SARC, nested packs, BFRES v3 FTEX, BFLIM) | Working — 1007 files, 14 572 textures (5 499 unique), 0 errors on the USA dump |
| GX2 decode (BC1–BC5, RGBA8, R8, RG8, 565, 5551, 4444; tiled/linear; mips) | Working; layout + tiling verified byte-exact against 14 561 of 14 572 textures |
| Classification (metadata + pixel statistics, manual override) | Working; rules, confidence, persisted overrides. No AI classifier |
| Upscaling | Lanczos/Catmull-Rom/Mitchell/bilinear/nearest in-process; Real-ESRGAN, waifu2x, Real-CUGAN, RealSR via the official ncnn/Vulkan releases; custom ncnn models |
| Post-upscale enhancement | Sharpening, edge-preserving detail recovery and colour (Oklab, gamut-mapped), per-class tuning, profiles Original / **Enhanced** (default) / Ultra Sharp / custom; alpha and data textures untouched; Enhance tab with split, side-by-side and variant comparisons; `enhance-bench` QA. See `docs/enhancement.md` |
| Normal maps, masks, ramps, alpha | Vector-aware normal resize + renormalise; masks conventional; ramps native; alpha resized separately |
| Mip generation | sRGB-linear box filter, alpha-coverage preserving, normal renormalising |
| Rebuild + validation | FTEX/BFLIM rewrite, SARC/Yaz0 rebuild, reopen + compare, PSNR check of every encoded texture |
| Game integration: rebuilt archives | Content packages for the recomp's mod manager. **Verified in game** with the memory-safe defaults: boots, title demo, Dragon Roost gameplay. The game's fixed-size heaps limit growth to ~2× per archive and rule out some archives (see `docs/design.md`) |
| Game integration: runtime pack | Studio side done: resolution policy (×2–×32, global / per class / per texture, steps down, never skips), `export-runtime`, `install --kind runtime`, mods-folder auto-detection. Recomp side implemented (`kind: textures`, `vr.texturePacks`) with unit and smoke tests. **Not yet verified in the game or the headset.** Use it or the archive packages, not both |
| Cube maps (8 textures), 3 textures with non-standard mip offsets, 1 BC5 SNORM normal map below the quality bar | Listed in Diagnostics; originals kept |
| Full 4K for every texture | Not possible by rebuilding archives (game heap sizes). The runtime pack has no such limit: a full export of the author's project is 4171 entries, 12.4 GiB estimated texture memory (see `docs/performance.md`); working in game is untested |
| PCVR BC7 packages | Exported by the runtime pack (BC1/2/3 → BC7, BC4, BC5, RGBA8 → BC7 or RGBA8 for UI and lettering); loading in game untested |
| Quest 3 & Steam Frame ASTC packages | Not exported; the runtime adapter loads BC formats only (see `docs/runtime-adapter.md`) |

## Build

Requirements: Rust (stable, 1.85+), a C/C++ toolchain (MSVC Build Tools on Windows, `base-devel` on Arch).

```bash
cargo build --release
```

The binary is `target/release/texture_studio(.exe)`.

### Arch Linux

```bash
sudo pacman -S --needed rust base-devel vulkan-icd-loader libxkbcommon wayland libx11 libxcursor libxrandr libxi
cargo build --release
```

### Upscaler tools (optional, for AI models)

The Windows builds of Real-ESRGAN (the default AI model) and waifu2x are included in `tools/upscalers/`, with their
licences. Real-CUGAN and RealSR are not (their model licences are not cleared); download them into `tools/upscalers/`
(each zip extracted into its own folder) if you want them. On Linux, download all of them:

| Tool | Official release | In this repository |
|---|---|---|
| Real-ESRGAN ncnn Vulkan (with models) | https://github.com/xinntao/Real-ESRGAN/releases/tag/v0.2.5.0 (`realesrgan-ncnn-vulkan-20220424-<os>.zip`) | Windows build |
| waifu2x ncnn Vulkan | https://github.com/nihui/waifu2x-ncnn-vulkan/releases/tag/20250915 | Windows build |
| Real-CUGAN ncnn Vulkan | https://github.com/nihui/realcugan-ncnn-vulkan/releases/tag/20220728 | no |
| RealSR ncnn Vulkan | https://github.com/nihui/realsr-ncnn-vulkan/releases/tag/20220728 | no |

Settings → **Install Upscaling Models** (or `texture_studio --headless install-tools`) downloads any of them that is
missing, from these official releases, checked against pinned SHA-256 sums. See `tools/upscalers/README.md`. Without
any AI tool, every class falls back to Lanczos-3 (logged). Custom ESRGAN-family ncnn models
(`name.param` + `name.bin`, 4×) can be placed in `tools/upscalers/custom-models/`.

## Use

1. Start `texture_studio`. Choose a **project folder** (output, cache, packages; needs several GB).
2. Choose the **game disc folder** (the one with `code/`, `content/`, `meta/`).
3. Press **▶ Process everything**: scan → extract → upscale → validate → export. Interrupted runs resume.
4. Review in **Assets** (approve/reject, change class, scale or model, compare original vs result) and
   **Compare models** (run several models on one texture, click the best).
5. **Export / install**: pick **Export runtime pack** or **Export rebuilt archives**, then install that kind into the
   recomp's `ModManager` folder (found the way the recomp finds it; changeable in Settings) and switch it on. Use one
   kind or the other, not both. Switching the packages off in the game's Mods tab restores the original textures.

See `docs/user-guide.md` for details and the headless commands (`texture_studio --headless …`).

## Documentation

- `docs/design.md` — feasibility study, integration decision, pipeline design
- `docs/formats.md` — supported resource formats, verification results
- `docs/dependencies.md` — every dependency, its licence and redistribution notes
- `docs/runtime-adapter.md` — the runtime adapter: what is implemented and where the design lives
- `docs/enhancement.md` — post-upscale sharpening, detail and colour: stages, defaults, profiles, benchmark
- `docs/user-guide.md` — GUI and command-line usage
- `docs/performance.md` — measured timings and memory

## Licence

The application code is MIT-licensed (see `LICENSE`). `third_party/wiiu_swizzle` is MIT (ScanMountGoat),
vendored with two documented patches.
