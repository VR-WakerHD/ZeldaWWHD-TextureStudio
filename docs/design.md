# ZeldaWWHD Texture Studio — feasibility and design

Date: 2026-11-02. Status: implemented 2026-11-02; integration A (rebuilt archives) verified in game with the memory-safe defaults (see below). Integration B (runtime pack) was implemented in both repos later the same day and is not yet verified in the game or the headset.

## Decisions taken with the user

| Question | Decision |
|---|---|
| GUI stack | Rust + egui/eframe. Builds on Windows (this RTX 5080 PC) and Arch Linux from one codebase. GTK4 was rejected because the development PC is Windows 11 (only a Fedora WSL distro is present). |
| Downloads | Official sources only: rustup, crates.io, upstream GitHub releases for upscaler binaries/models. Each recorded in `docs/dependencies.md`. |
| Game integration | Rebuilt archives delivered as recomp **content packages** first. No change to ZeldaWWHD-VR. A runtime replacement adapter followed (B below) as a second way to deliver; the user uses one or the other. |
| Game testing | Short flat (desktop) launches allowed on 2026-11-02 to verify loading. No VR performance tests. |
| Upscalers | Implement as many official upscalers as practical so quality can be compared on real assets. |

## Findings: how the game stores and loads textures

Checked against the user's dump at `ZeldaWindWakerRecomp/game_disc` and the recomp source.

- `content/Common/**/*.szs` (≈800 files) are **Yaz0-compressed SARC** archives (big-endian, `FE FF` BOM).
- Models are **BFRES version 3.4.0.1** (Wii U FRES, big-endian) inside those SARCs, e.g. `Object/Bgn.szs/Bgn.bfres`.
  Textures are `FTEX` entries in the BFRES texture index group; each embeds a GX2Surface (format, tile mode,
  swizzle, pitch, alignment, 13 mip offsets) plus five precomputed GX2 texture registers.
- Layouts (`Common/Layout/*.szs`) hold **BFLIM** images (`timg/*.bflim`): raw GX2 data followed by a `FLIM` +
  `imag` footer.
- Shaders are `.sharcfb` (not textures). Other containers (`Pack/*.pack`, `Particle`, `ProgramTexture`, `Jpeg`)
  are inventoried and handled only once their formats are verified; unsupported textures are listed, never
  silently skipped.
- The recomp detiles GX2 surfaces with Cemu's LatteAddrLib (MPL-2.0) and translates GX2 formats to host formats
  (`runtime/src/gfx`, `runtime/third_party/cemu`). It does **not** have a loose-texture override mechanism.
- The recomp mod manager supports **content packages** (`docs/mod-manager.md`, `runtime/src/mods/content.cpp`):
  game-relative files under `content/` replace the originals on read-only opens; missing paths fall through to the
  disc. Enforced limits at startup: **4096 entries, 128 MiB per file, 512 MiB per package**; conflicting enabled
  packages (same path) are rejected.

## Integration: comparison

| | A. Rebuilt archives as content packages | B. Runtime texture replacement |
|---|---|---|
| Game changes | None | New loader in the recomp's GX2 texture path |
| Formats possible | GX2 formats only: BC1–BC5, RGBA8, etc. **No BC7/ASTC** (GX2 has none) | Any host format (BC7, ASTC on Quest) |
| Fallback | Disable the package → original files | Per texture |
| Guest memory | Upscaled data lives in the game's 1 GB guest RAM and archive heaps — the main risk | Only host VRAM |
| Updating | Re-export affected archives | Swap single files |
| Status | Implementable now | Separate coordinated project |

**Chosen first: A**, because it needs no game change and uses the existing, tested mod loader. B was built
afterwards for what A cannot do (full-scale textures, BC7 on PCVR): a new mod-manager package kind `textures` in
the recomp, with the design in `docs/superpowers/specs/2026-11-02-runtime-texture-adapter-design.md` and a status
note in `docs/runtime-adapter.md`. B has no ASTC output yet. B is implemented and unit/smoke-tested; **it is not
yet verified in the game or the headset**. A and B replace the same textures in different ways, so only one of
them should be switched on (the install step warns when both are).

Consequences of A:
- Output formats keep each texture's **original GX2 format** (BC1 stays BC1 etc.). The "PCVR BC7" and "Quest
  ASTC" outputs require B and are exported only as lossless masters plus DDS/KTX2 previews, clearly labelled as
  not loadable by the game yet.
- **Guest memory budget — verified to be the real limit.** The game loads archives into heaps whose sizes are
  fixed in its code. Tested in the recomp on 2026-11-02:
  - Enlarging `Misc.szs` (1 MiB expandable heap created by the boot task), `Tlogo.szs`, `Particle.szs`,
    `ProgramTexture.szs`, `first_szs_permanent.pack` or `permanent_2d_*.pack` crashes or halts the game at boot.
  - Packs mount their inner archives on demand into a resident heap: an enlarged `Ship.szs` inside
    `szs_permanent0.pack` halts the game in `d_a_ship.cpp` (`modelData != 0`) when gameplay starts.
  - Unlimited growth (full ×4) of stage/object archives boots, but a later title-demo scene crashed.
  - **Working default:** every archive may grow to at most 2× its original decompressed size, and the archives
    above plus all `Common/Pack/` are never rebuilt. With this, 5 531 textures in 670 archives were replaced;
    the game booted, ran the title demo for 4.5 minutes, loaded Quest Log 1 on Dragon Roost Island and stayed
    stable while walking and talking to the King of Red Lions.
  - Full 4K everywhere therefore needs either the runtime adapter (approach B, no guest-heap limit) or a recomp
    change that enlarges the game's heaps.
- Packages are split into several content packages of ≤ 512 MiB / ≤ 4096 files, with no overlapping paths.

## Rebuild method (A) — safety rules

1. Never write into the disc directory. Everything goes to the project's output directory.
2. FTEX rewrite: new tiled image/mip data is **appended** at the end of the BFRES (aligned to the surface
   alignment and the FRES alignment). Only the FTEX's surface fields, texture registers and its two data pointers
   change; every other offset in the file stays valid. Header file size is updated.
3. Surface parameters (pitch, alignment, image/mip sizes, mip offsets, per-mip tile modes) are computed with
   addrlib (`wiiu_swizzle`). **Validation gate:** for every original texture on the disc, recomputing its own
   parameters must reproduce the stored values, and tile → detile must round-trip byte-exactly. Formats or tile
   modes that fail the gate are not rewritten (reported instead).
4. Texture registers are recomputed with the same rules as GX2InitTextureRegs; the same gate applies (recomputed
   registers for original textures must equal the stored ones).
5. SARC rebuild keeps file order, name hashes, the hash key and each file's data alignment; Yaz0 recompression
   keeps the original header's alignment field.
6. After writing, the archive is reopened by the same parser and its inventory compared (names, counts, formats,
   new dimensions) before it is placed in a package.

## Pipeline

```
scan → inventory (blake3 hashes) → extract (decode + detile → lossless PNG master + JSON metadata)
     → classify (metadata/name rules, confidence, manual override)
     → policy (target size per texture, budget)
     → upscale (backend per class) → review (approve/reject)
     → encode (original GX2 format, mip chain per class) → rebuild BFRES/BFLIM → SARC → Yaz0
     → validate (reopen) → content packages
```

Every stage is keyed by content hash + settings hash + backend/model version, so finished work is reused and
anything whose inputs changed is redone. State is a JSON-lines job log plus per-texture records in the project
directory; an interrupted run resumes.

## Upscaling backends

One trait, `Upscaler`: `upscale(rgba, scale) -> rgba`. Implementations:

- Conventional (in-process): nearest, bilinear, Catmull-Rom, Mitchell, Lanczos3 (`fast_image_resize`).
- ncnn/Vulkan executables from the upstream projects' official releases, run as child processes in folder-batch
  mode (model loaded once per batch, GPU by default, `-t` tiling, CPU fallback via `-g -1`):
  Real-ESRGAN (`realesrgan-ncnn-vulkan`: x4plus, x4plus-anime, animevideov3 ×2/×3/×4), waifu2x
  (`waifu2x-ncnn-vulkan`: cunet, upconv_7 anime/photo), Real-CUGAN (`realcugan-ncnn-vulkan`), RealSR
  (`realsr-ncnn-vulkan`). Any user ncnn model folder can be added as a custom model.
- Alpha: RGB and alpha are upscaled separately (alpha via conventional filter by default) and recombined, so AI
  models never see premultiplied edges or produce halos.
- Normal maps: never sent to AI models; decoded to vectors, resized with Catmull-Rom, renormalised, re-encoded.
- Masks/UI/lettering: conventional filters only by default.

ncnn was chosen over ONNX Runtime/TensorRT because the upstream projects publish ready ncnn models and Vulkan
binaries for Windows and Linux; ONNX would need PyTorch conversions of the `.pth` weights and a CUDA/cuDNN
install that this PC does not have.

## Crate layout

```
crates/wwhd_tex       library: formats (yaz0, sarc, bfres, bflim, gx2), scan, extract, classify, policy,
                      upscale, encode, rebuild, package, jobs/cache
crates/texture_studio egui application + `--headless` CLI for scripted runs and tests
```

## Out of scope for this iteration

- ASTC packages for Quest 3 / Steam Frame (the runtime adapter loads BC formats only).
- AI-based classification (metadata/name/statistics rules first; listed as an option).
