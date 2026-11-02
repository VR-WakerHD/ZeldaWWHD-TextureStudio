# Runtime texture replacement for ZeldaWWHD-VR — design

Date: 2026-11-02. Status: design, awaiting review. Supersedes the outline in `docs/runtime-adapter.md`.

## Goal

Show Texture Studio's upscaled textures (×2 to ×32 by the resolution policy below) everywhere in the game, without rebuilding game
archives. The game's own data, guest memory and archive heaps stay exactly as on the disc; only what the GPU
samples changes. This removes the heap limit found on 2026-11-02 (archives may grow ≤ 2×, `Common/Pack/` and four
other archives excluded), which is why the rebuilt-archive packages reach only 5 531 of 14 572 textures.

### Decisions (user, 2026-11-02)

| Question | Decision |
|---|---|
| Route to true 4K | Runtime adapter in the recomp (not larger game heaps) |
| Platforms, first version | PCVR: the Vulkan renderer on Windows. Android/Quest (ASTC) and Metal are left out, but nothing here rules them out |
| First sight of a replaced texture | The original is drawn until the replacement is loaded in the background, then it switches (no frame hitch, a brief pop) |
| Design | B: a separate replacement surface substituted at bind time (below) |
| Resolution policy | Scales ×2, ×4, ×8, ×16, ×32; set globally, overridable per class next to the model choice (below) |

### Requirements carried over

- Graphics stay exact wherever no replacement applies; no frame-rate loss in the headset (target 90 fps).
- The game's logic rate and guest state never change.
- Off unless a texture package is enabled in the mod manager.
- Any mismatch or error → the original texture, never a crash or a wrong image.
- In-game and headset tests only with the user's go-ahead, at Capped 90, reporting average fps and % at 90.
- Nothing committed or pushed without the user's explicit OK.

### Non-goals

- Android/Quest, Metal, ASTC output (later).
- Replacing textures the game renders or writes on the CPU at run time (render targets, video, dynamic UI).
- Cube maps and 3D/array textures (first version: 2D, one slice).
- Changing the rebuilt-archive packages; they keep working as before.

## Overview

```
Texture Studio                              ZeldaWWHD-VR (Vulkan)
──────────────                              ─────────────────────
masters + upscales ─► Runtime pack export   mod manager: kind "textures" ─► texture_pack::load (index only)
                      DDS (BC7/BC5/BC4,                                          │
                      full mips) + index    upload_surface: real upload ─► key ─► lookup ─► loader thread
                                                                                          (reads DDS)
                                            frame start (render thread): create image, upload, mark ready
                                            resolve_texture: unit allowed + key still current
                                                             ─► bind replacement surface (sx = sy = scale)
```

## Pack format

A mod-manager package of a new kind, `textures`:

```
ModManager/Mods/<id>/
  manifest.json       { "format_version": 1, "id", "name", "version", "game_id", "kind": "textures",
                        "textures_dir": "textures" }
  textures/index.json
  textures/<aa>/<key>.dds
```

`index.json`:

```json
{ "format_version": 1, "generator": "ZeldaWWHD Texture Studio <version>",
  "entries": [ { "format": 49, "width": 256, "height": 256, "hash": "9f2c…16 hex…",
                 "scale": 4, "file": "9f/9f2c….dds", "name": "Bgn.szs/Bgn.bfres/sand00" } ] }
```

- **Key** = (GX2 surface format, width, height, XXH3-64 of the level-0 guest bytes). In WWHD the game places the
  FTEX/BFLIM image data in guest memory as stored, so level 0's bytes are the file's image data
  (`imageSize` bytes; Texture Studio's gate checked that addrlib's level-0 size equals the stored size for 14 561
  of 14 572 textures). Texture Studio computes the key offline; the recomp gets it from the hash it already
  computes (below). Identical textures in several archives share one entry.
- **scale** ∈ {2, 4, 8, 16, 32}; the DDS's level 0 is exactly `width·scale × height·scale`, with
  `original mips + log2(scale)` levels (Texture Studio's mip rules: sRGB-linear, alpha coverage, normals
  renormalised).
- **DDS formats** keep the original's meaning, so the guest's component selectors, sRGB flag and signedness still
  apply unchanged:

  | Original GX2 format | Replacement |
  |---|---|
  | BC1 / BC2 / BC3 (UNORM or SRGB) | BC7 UNORM or BC7 SRGB, the same as the original |
  | BC4 UNORM / SNORM | BC4 of the same signedness |
  | BC5 UNORM / SNORM | BC5 of the same signedness |
  | RGBA8 UNORM / SRGB | BC7 of the same colour space; RGBA8 when Texture Studio's class is UI or lettering (no compression loss on text) |
  | anything else | no entry (stays original) |

- Duplicate keys in one package, or the same key in two enabled packages: the first package in load order wins,
  and the mod manager shows a warning (as content conflicts do today, but not fatal).

## Recomp changes

### 1. Mod manager (`runtime/src/mods/packages.cpp`)

- Accept `kind: "textures"` with a required `textures_dir` (a relative path, no symlinks, the same checks as
  `content_dir`).
- Restart required to switch on or off, like `content`.
- Expose the enabled texture packages' folders to the renderer in load order (`packages::texture_roots()`).

### 2. `runtime/src/gfx/texture_pack.{h,cpp}` (new, renderer-independent)

- `load(roots)` at renderer start: parses each `index.json` into one hash map `Key → Entry{path, scale, format}`.
  Size: ~15 000 entries, a few MB. Errors in one package disable that package (logged), never the game.
- `lookup(key)`: a pure lookup, unit-tested.
- A loader thread with a queue: reads and validates a DDS (magic, DX10 header, format matches the table above,
  level-0 size = guest size × scale, scale a power of two from 2 to 32, level count, file size) and hands back the
  level data. No GPU work on this thread. An entry whose level 0 exceeds the device's `maxImageDimension2D`
  (16384 on current desktop GPUs) is marked bad at load and skipped.
- `texture_pack_core.h`: the DDS header check and the key/format rules as pure functions for
  `runtime/tools/texture_pack_test.cpp`.

### 3. Detection (`runtime/src/gfx/vulkan/surfaces.cpp`, `upload_surface`)

`upload_surface` already hashes every level with XXH3 (`content_hash`) whenever the guest's bytes may have
changed. The level-0 hash is one of those terms; it is kept as `s->level0Hash` at no extra cost.

After a real upload (contents changed), if packs are loaded, the surface is a candidate (not depth, not
`gpuWritten`, 2D, one slice, a format in the table) and its key is in the map:

- If `s->replacement` exists for this key → nothing to do.
- Otherwise drop any old replacement and queue a load for the key.

Contents changed to something not in the map → the replacement is dropped; the original draws.

### 4. Creating the replacement (render thread, frame start)

At the start of each frame, under the surface lock, finished loads are turned into images (bounded by a byte
budget per frame, default 64 MiB, so a burst of new textures spreads over frames):

- `s->replacement`: a `Surface` owned by the guest surface (as `mipChain` is), with extent ×scale, `mips` =
  DDS levels (at most 15 at 16384 px, within the renderer's 16-level arrays), host format from the DDS, `sx = sy = scale`, `lodOffset = log2(scale)`, `gpuWritten = false`, and the
  key it was made for.
- Uploaded through the existing staging path (`texture_staging_slice`); layout → shader read.
- Not in the guest address maps, so no copy, render-target lookup, eye sharing, invalidation or alias code ever
  sees it.
- A load whose surface has gone or whose key changed meanwhile is discarded.
- Lifetime: destroyed with its guest surface (invalidation, save-state reset, `ss_reset_surfaces`). The texture
  budget's eviction of the guest image (`WWHD_VK_TEXTURE_BUDGET`) does not touch the replacement; restoring the
  guest image re-uploads and re-hashes it, and the unchanged key keeps the replacement.

### 5. Binding (`runtime/src/gfx/vulkan/draw.cpp`, `resolve_texture` and the sampler)

`resolve_texture` gets the guest surface as today, then substitutes the replacement when **all** of the
following hold:

- the replacement exists and its key equals the guest surface's current key (`level0Hash`, format, size);
- the guest surface is not `gpuWritten` and not bound as a render target in this draw;
- the shader does not use texel coordinates (`texelFetch`) or size queries (`textureSize`) on this unit. The
  decompiler emits `uf_tex{N}Scale` only for units sampled with texel coordinates, and `LD` with an explicit level
  would need a level offset; size queries would return the larger size. Such units keep the original;
- the shader does not query the size or LOD of this unit (`GET_TEXTURE_RESINFO`, `GET_COMP_TEX_LOD`), and does not
  sample it at an explicit non-zero LOD (`SAMPLE_L`, `SAMPLE_C_L`: a guest level would land `lodOffset` levels
  finer). `SAMPLE_LZ` (level 0) is fine. The decompiler's analyzer gets a per-unit flag
  `textureNeedsGuestLevels[]` beside `textureUsesDepthCompare[]` for these and for texel coordinates;
- the guest surface is not flagged `artisticMips` (hand-drawn levels; Texture Studio regenerates mips by
  downscaling, which would replace them).

The sampler made for a replacement adds `lodOffset` to MAX_LOD, and to MIN_LOD when MIN_LOD > 0 (guest level 0
is replacement level `lodOffset`; finer levels stay reachable). With no mip filter the sampler reads level 0
only, as on the original; no offset. The sampler cache key includes the offset. LOD
bias is unchanged: normalized coordinates on the larger image pick the matching finer level by themselves.
Anisotropy and component selectors work unchanged on the replacement surface.

Threads (scene-B, atlas): they only bind replacements that are already created; creation happens only at frame
start on the render thread, so no new synchronisation is needed beyond the existing surface lock.

### 6. Memory

- Replacement images are counted separately (`texture_pack` VRAM total, shown in the stats line and F1 overlay).
- Budget: `vr.texturePackBudgetMB`. Default, read once after the VR session has started (device-local heap,
  `VK_EXT_memory_budget`): min(budget / 2, budget − usage), at least 1 GiB. Over budget, replacements not bound for 300 frames are released least recently used
  first (reuses the pure `texture_budget::select`). A released one reloads on next use (the original draws
  meanwhile).
- The loader holds at most 256 MiB of finished-but-not-uploaded data; beyond that it waits.

### 7. Settings and diagnostics

- `vr.texturePacks` (on/off, default on; does anything only with an enabled `textures` package). Restart applies.
- `WWHD_TEXTURE_PACK_LOG=1`: one line per hit, miss with a near match (same format and size), load, release.
- No tint code in the recomp: for the visual check, Texture Studio's existing debug-tint option is applied to the
  runtime pack export (see testing).
- Stats line: hits, loads pending, VRAM used / budget, units kept original due to texel fetches.

## Resolution policy (Texture Studio)

Applies to both exports (runtime pack and rebuilt archives).

**Scales:** Native (×1), ×2, ×4, ×8, ×16, ×32.

**Where it is set, most specific first:**

1. Per texture: the existing scale override in Assets (extended to ×32).
2. Per class: a **Scale** column next to **Model** in Settings → *Model per class*, with the choices
   *Global*, Native, ×2 … ×32. Default *Global* for every class, except the classes that are native today
   (lighting ramps, unsupported), which keep Native.
3. Global: Settings → *Resolution policy* → **Scale** (default ×4, as today).

**Maximum side:** Settings → *Resolution policy* → **Max side** with the choices 2048, 4096 (default), 8192 and 16384.
- A texture's scale is halved until its larger side fits (the step-down rule below). So the full ×32 reaches only
  small textures; at 4096, textures of 128 px and below get ×32, and a 512 px texture gets ×8.
- The existing per-archive budget halves further, for rebuilt archives only. It is the one exception to the
  step-down rule: it may still leave textures original, because it keeps the archive within the game's heap and
  forcing ×2 crashes the game. The runtime pack has no such budget.
- The plan shown in Assets says which rule set the scale ("class: ×8", "capped by max side 4096 → ×4").

**Step down, never skip (user, 2026-11-02).** A texture whose chosen scale can't be reached gets the largest
scale below it that works (×32 → ×16 → ×8 → ×4 → ×2). It is not left original. This applies to every limit:

- max side;
- the device limit of 16384;
- the per-archive budget (rebuilt archives only, down to ×2; below that see the exception under Maximum side);
- a model run that fails;
- an AI result that fails the consistency check. The same model is tried again at the next lower scale.

Only after ×2 has failed with the class's model is a conventional filter used, at the largest scale the size
limits allow.

A texture stays original only when even ×2 is impossible: its larger side is already at the max side, or its class
or the texture's own override is Native. The old rule that textures of 2048 px and above always stay native is
dropped. The max side alone decides.

The plan and the upscale report record each step down and its reason, for example "×32 wanted → ×8: max side
4096" or "×16 failed consistency (19 dB) → ×8".

**Mips:** original + log2(scale), up to five more levels at ×32.

**Upscaling above a model's own scale:**
- Models run at their native ×4 or ×2 in passes:
  - ×8 = ×4 then ×2
  - ×16 = ×4 then ×4
  - ×32 = ×4, ×4, then ×2
  - Where a backend has no ×2, it runs ×4 and is shrunk by Lanczos.
- Each pass keeps the 16 px wrap/clamp border.
- The existing consistency check compares the final result, shrunk back, with the original. Below 22 dB the
  texture steps down a scale with the same model (above). It uses Lanczos only after ×2 has failed.
- Conventional filters and the normal-map path resize in one step at any scale.

**Size warnings:**
- The Dashboard and Settings show, per class and in total, the texel count, disk size and estimated VRAM at the
  chosen policy.
- The runtime-pack export warns when the estimate exceeds 8 GiB. It doesn't refuse; the recomp's budget and LRU
  bound what is resident.

**Caches:** upscales are already keyed by scale (`<hash>_x<scale>.png`), so changing the policy reuses what exists
and only runs the new scales.

## Texture Studio changes

- `wwhd_tex::runtime_pack`:
  - key: XXH3-64 (crate `xxhash-rust`, `xxh3` feature, MIT/BSL-1.0) over the stored level-0 image bytes;
  - DDS writer (DX10 header) for BC7/BC4/BC5/RGBA8 with the mip chain from `mips::chain`, encoded with
    `image_dds` (BC7 at "slow" quality);
  - format table as above.
- Export → **Runtime pack** (GUI) and `--headless export-runtime` (CLI): no per-archive budget and no exclusions
  (they existed for guest heaps); split into packages of ≤ 4 GiB; writes `manifest.json` (`kind: textures`).
- The install step enables the texture packages and warns when rebuilt-archive packages are enabled too (their
  textures no longer match the keys; harmless, but wasted memory).
- Encode cache shared with the archive export (same blake3 cache keys plus the output format).

## Errors and fallbacks

| Problem | Result |
|---|---|
| Bad or missing `index.json` | that package off, logged, shown in the Mods tab |
| DDS missing, wrong size/format/levels, truncated | entry marked bad for the session, original used, logged once |
| Vulkan image creation fails (out of memory) | release LRU replacements, retry once, then original for that texture |
| Guest contents change (the game rewrites a texture) | the key no longer matches; original from the next draw |
| Shader uses texel fetch / size query on the unit | original for that draw |
| `vr.texturePacks` off or no enabled package | no lookups, no loader thread: identical to today |

## Performance

- Per draw: one pointer check and one 64-bit compare per bound texture with a replacement; nothing when packs are
  off.
- Per upload: one hash-map lookup (the hash is already computed).
- Disk reads and DDS validation on the loader thread; GPU uploads spread over frames with the per-frame budget.
- VRAM: BC7 at ×4 is 16 bytes per 4×4 block, i.e. 1 byte per texel; a 1024² original becomes 16 MiB + mips
  (~21 MiB). The working set of one area is what counts; it is measured in the first test and the default budget
  adjusted.

## Testing

Unit (no GPU, no game):

- `runtime/tools/texture_pack_test.cpp`: index parsing (good, duplicate keys, bad entries), key rules and format
  table, DDS header checks, sampler LOD offset, the bind rule's conditions (pure function over flags).
- Texture Studio: XXH3 key equals the recomp's for real disc textures (the recomp's `XXH3_64bits` and the Rust
  crate implement the same algorithm; a fixture of 20 real textures with their keys checked from both sides),
  DDS round-trip (write, read back, compare levels), format table; resolution policy: precedence (texture >
  class > global), the max-side cap, pass plans for ×8/×16/×32 (with and without a ×2 model), mip counts.

Game (each run only after the user's go-ahead):

1. Desktop build, tinted pack: replaced textures appear magenta in the title scene, Outset and Dragon Roost;
   untinted ones unchanged.
2. Headset, Capped 90, real pack: the same route; report average fps and % of frames at 90, VRAM used, pops seen.
   Fail rule as usual (< 35 fps average → stop and close the game).

## Rollout

1. Recomp: mod-manager kind + `texture_pack` core + unit tests.
2. Recomp: detection, loader, creation, binding, sampler offset, budget.
3. Texture Studio: resolution policy (global + per class, ×2–×32, multi-pass upscaling), runtime pack export,
   key fixture test.
4. Desktop tinted test (ask), then the headset test (ask).

## Risks

- **Texel-fetch units.** Textures sampled that way stay original. If many of the game's colour textures turn out
  to be fetched that way, a level offset for `LD` could be added to the shader translation later.
- **Hash identity.** Assumes the game uploads a texture's bytes unchanged from the archive. The near-match log
  (same format and size, other hash) shows where that is false.
- **VRAM in VR.** The headset path already uses much of the 16 GB; the budget and LRU keep the pack bounded, and
  the first headset test measures the real working set.
- **Stacked AI passes.** ×8 and above feed a model its own output, which can amplify its artefacts (oversharpened
  edges, invented texture). The end-to-end consistency check catches gross failures only; quality has to be
  judged in Compare models, which gets the new scales too.
- **Size at large scales.** ×8 is 64× the texels and ×32 is 1024×; with max side 8192 or 16384 a single texture
  reaches 64–256 MiB of BC7 with mips. The max-side cap, the export warning and the VRAM budget keep this in
  check, but disk use and upscale time grow fast (a 512 px texture at ×16 is one 8192² image per pass).
- **Pop-in.** A texture seen for the first time draws in original quality for a few frames. Acceptable per the
  user's choice; the per-frame upload budget trades pop duration against frame time.
