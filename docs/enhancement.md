# Post-upscale enhancement

Sharpening, detail recovery and colour, applied to each upscaled texture when it is exported (rebuilt archives and
runtime packs alike). Code: `crates/wwhd_tex/src/enhance.rs`; GUI: the **Enhance** tab; QA: `enhance-bench`.

## Where it runs

```
master PNG (lossless original)
  → upscale (AI or filter) → cache/upscaled/<backend>/<hash>_x<scale>.png   ← lossless master of the upscale, never changed
  → enhance (this stage, in memory)
  → mips (box, or Catmull-Rom with "sharper mips")
  → encode (BC7 / BC4 / BC5 / RGBA8 for runtime packs; the original GX2 format for archives)
```

The upscaled PNG stays untouched, so changing a profile only re-encodes. Encode caches are keyed by the profile, the
texture's class/confidence/format/scale/edge mode and the image bytes, so a changed profile never reuses stale
output and an unchanged one never re-encodes.

## What it never touches

- **Alpha**: returned byte for byte. Filters weight neighbours by alpha, so colour hidden under transparent texels
  cannot bleed into edges, and texels with alpha 0 keep their colour (unless *Fill colour under transparent
  texels* is on: cut-out textures only, off by default because in some textures alpha is not coverage).
- **Data textures**: normal maps, masks, ramps, and any one- or two-channel format (BC4, BC5, R8, RG8) are never
  enhanced, whatever their class says.
- **Unknown class**: not enhanced (it may be data). Set the class by hand to enhance one.
- **UI and icons**: lightly sharpened; colours unchanged (×0 colour tuning).
- **Dimensions, names, hashes, index entries, DDS formats**: unchanged. Enhancement changes pixel values only.

## Colour handling

Pixels are read as sRGB-encoded and converted to Oklab. Sharpening and detail touch Oklab lightness only, so they
cannot create colour fringes. Saturation, vibrance, contrast and so on act on lightness and chroma. Colours pushed
out of the sRGB gamut lose chroma, keeping hue and lightness, instead of clipping. The conversion back uses the same
curve, so a no-op profile returns the exact input bytes. This also holds for the game's UNORM colour textures: their
values are painted, gamma-encoded colour too, and the round trip is exact either way.

*Highlight clipping protection* fades brightening out over the top 10 % of lightness. *Shadow detail protection*
does the same for darkening over the bottom 10 %. Both apply to the total change, sharpening included.

## Stages and defaults

| Parameter | Original | **Enhanced (default)** | Ultra Sharp |
|---|---:|---:|---:|
| Unsharp mask radius (source texels) | 0.6 | **0.6** | 0.7 |
| Sharpening amount | 25 % | **55 %** | 85 % |
| Threshold (8-bit levels) | 3 | **2** | 2 |
| Edge protection / halo suppression / adaptive | on | **on** | on |
| Detail recovery (local contrast) | off | **30 %** | 55 % |
| Detail size (source texels) / edge level | — | **0.75 / 0.05** | 0.75 / 0.05 |
| Texture restoration | off | **0 %** | 15 % |
| Saturation | 0 | **+7 %** | +8 % |
| Vibrance | 0 | **+5 %** | +8 % |
| Contrast | 0 | **+3 %** | +4 % |
| Gamma | 1.0 | **1.0** | 1.0 |
| Highlight / shadow protection | on | **on** | on |
| Per-class tuning | on | **on** | on |
| Sharper mips (Catmull-Rom) | off | **on** | on |

**Radius unit.** The radius counts *source texels* by default, so ×4 output is sharpened with a 2.4 px sigma. In
output pixels, 0.6 px on a ×4 texture is a quarter of a painted texel: the benchmark measured +3 % sharpness, and
mips remove even that whenever a surface is further away than arm's length. Untick *radius in source texels* for
output pixels.

**Sharpening** is an unsharp mask on lightness. *High-pass* mode instead weights the detail by a mid-tone mask, which
leaves highlights and shadows alone. *Threshold* keeps flat cel shading flat. *Edge protection* sharpens strong
outlines up to 60 % less than fine detail. *Adaptive* measures how busy the texture already is: clean art gets up to
+15 %, noisy art down to −40 %.

**Halo suppression.** After detail and sharpening, every texel is clamped to the 5×5 neighbourhood range of the
unenhanced image, plus about one 8-bit level. Edges get steeper, but no rim can form on either side of them. Before
this clamp, a 2.4 px sigma produced visible light rims along dark outlines (halo measure 9 %). With it, the measure
is 0.03 % for sharpening alone.

**Detail recovery** is a self-guided filter on lightness. Regions whose local contrast is below the edge level count
as painted texture and are lifted; edges above it are left alone. *Texture restoration* puts back the original's own
band-pass detail where an AI model flattened it. It invents nothing, but it also brings back the original's BC1 block
noise, so keep it low.

**Sharper mips** reduce each colour mip level with Catmull-Rom (in linear light for sRGB formats) instead of a 2×2
box. Normal maps and data always use the box. Alpha-tested coverage preservation still applies.

## Per-class tuning

| Category (from class + alpha) | Sharpen | Detail | Colour | Why |
|---|---:|---:|---:|---|
| Character artwork | ×1.0 | ×0.8 | ×1.0 | soft painted gradients on faces and cloth |
| Environment artwork, terrain | ×1.0 | ×1.0 | ×1.0 | |
| Vegetation | ×0.75 | ×0.8 | ×1.0 | over-sharpened foliage shimmers in VR |
| Water | ×0.5 | ×0.4 | ×0.6 | smooth gradients; the sea colour defines the look |
| Stone & architecture (rock, wood) | ×1.15 | ×1.25 | ×0.9 | painted texture benefits most |
| Sky & clouds | ×0.3 | ×0.2 | ×0.8 | gradients show banding and noise |
| UI & icons | ×0.6 | ×0 | ×0 | colours must stay exact; text must not ring |
| Effects (particles) | ×0.4 | ×0.3 | ×0.6 | soft by design |
| Transparent (blended alpha) | ×0.7 | ×0.6 | ×0.9 | halos show first |
| Unknown, data | not enhanced | | | |

Classes below 50 % confidence get ×0.6 on top of this. Classes set by hand do not. *Tune per texture class* off
gives every colour texture the same strengths (UI colour stays unchanged).

## Profiles

Wind Waker Original, Enhanced (the default) and Ultra Sharp are built in and fixed. In **Enhance**: *Duplicate*
makes an editable custom profile; the name field renames it; *Reset to Enhanced* restores defaults and keeps the
name; *Delete* removes it; *Save to file… / Load from file…* exchange profiles as JSON. Custom profiles live in the
project's `settings.json` (`enhance.custom`). Each texture can pick its own profile (stored in
`manifests/review.json` as `profile`). Without one it follows the project's profile.

## Comparing

**Enhance** shows the texture selected in Assets:

- **Split**: drag the yellow divider. **Side by side**: two synchronised panes. Each side can show the original (its
  own texels), the AI upscale, the enhanced result, or the result as exported (encoded to the real BC format and
  decoded).
- **Variants**: synchronised grids of stages, profiles, sharpening strengths ×0–×2, colour strengths ×0–×2, or every
  cached model with the active profile.
- Mouse wheel zooms at the cursor, dragging pans, double-click (or **Fit**) fits, and **100 %** shows one texel of
  the target resolution per screen pixel. Previews are computed on a worker thread from the cached upscale; only the
  images on screen are computed.

## Models

The built-in models are unchanged. Real-ESRGAN `realesrgan-x4plus-anime` is the RealESRGAN_x4plus_anime_6B model.
`4x-AnimeSharp`, `4x-UltraSharp` and `4x-IllustrationJaNai` are known by name. They are not bundled: their
licences are non-commercial and they are published as PyTorch files. Converted to ncnn (`<name>.param` + `.bin`,
ESRGAN/RRDB builds only), they go in `tools/upscalers/custom-models/` and run through `realesrgan-ncnn-vulkan`. A
custom model counts as installed only when both files are there. Each class lists suggested models, but the class
backend stays whatever you chose. Compare models before switching: a more aggressive model is not assumed to be
better.

The ×2-with-a-×4-model path already upscales ×4 and then reduces with Lanczos. Running two AI passes to supersample
a ×4 target is not offered, because the second AI pass works on AI output.

## Benchmark

```bash
texture_studio --headless enhance-bench --project DIR [--per-class N | --names A,B] [--backend KEY] [--scale 4] [--crop 40] [--max-side 512]
```

This writes `reports/enhance_bench/report.md`, `report.json` and one contact sheet per texture (4 per row:
original, AI upscale, +sharpen, +colour, +sharpen+colour, Enhanced, Original profile, Ultra Sharp, then every other
cached model). It measures sharpness (Sobel of Oklab L), colourfulness (Hasler–Süsstrunk), chroma, ΔE and lightness
shift against the upscale, halo share, fidelity (the variant reduced to the source size, SSIM/PSNR against the
original), alpha integrity, and time.

Results from 2026-11-04 (18 textures, 2 per class, Real-ESRGAN anime ×4, 24 CPU threads, RTX 5080):

- Enhanced: sharpness +4 % (water, clouds) to +34 % (stone); colourfulness +5–15 %; mean ΔE 0.1–1.3; fidelity SSIM
  down by at most 0.006; halo share ≤ 1.4 %; alpha and hidden colour byte-identical in every variant; the mask
  untouched.
- Cost: enhancement 41 ms per megapixel on the CPU (about 4 % of the existing mips + BC7 encode, 1.09 s/Mpx). The
  benchmark process peaked at 428 MiB. The stage uses no GPU, so it adds no VRAM and needs no CPU fallback.
- GPU upscaling (Real-CUGAN se ×4, 15 textures): 6 s, GPU 82–88 % busy, VRAM +2.2 GiB over the desktop baseline.

## Known limits

- The metrics catch regressions; judge quality by eye in Enhance. Sharpness gains are partly invisible once a
  surface is far enough away to use lower mips; sharper mips are what helps there.
- Contrast moves lightness across the whole texture, so the halo measure counts some texels that are not halos.
- Texture restoration also restores the original's compression noise.
- The game has not been run with enhanced packs yet (picture, VRAM, shimmer in the headset).
