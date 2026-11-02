# Runtime Texture Adapter + Resolution Policy Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** ZeldaWWHD-VR (Vulkan, PCVR) draws Texture Studio's upscaled textures from a `textures` mod package at
run time, and Texture Studio exports such packages with a ×2–×32 resolution policy set globally, per class and
per texture.

**Architecture:** The recomp keeps every guest texture exactly as today. When `upload_surface` sees level-0
bytes whose key is in an enabled pack, a loader thread reads the DDS. At the next `swap()` the render thread builds
a separate replacement `Surface` (extent ×scale, `sx = sy = scale`). `resolve_texture` binds it in place of the
guest surface only while the key still matches and the shader unit needs no guest levels; the sampler's LOD range
is shifted by log2(scale). Texture Studio computes the same key (XXH3-64 of the stored level-0 bytes) and writes
DDS + `index.json`.

**Tech Stack:** C++20, Vulkan, CMake/Ninja (llvm-mingw), xxHash (vendored); Rust 2021, egui 0.37, `image_dds`
0.7, `xxhash-rust` (xxh3).

**Spec:** `ZeldaWindWakerHDto4K\docs\superpowers\specs\2026-11-02-runtime-texture-adapter-design.md`

**Repos:**
- Recomp: `ZeldaWindWakerRecomp\ZeldaWWHD-VR` (paths below marked `VR:`)
- Studio: `ZeldaWindWakerHDto4K` (paths marked `TS:`)

## Global Constraints

- Graphics stay exact wherever no replacement applies; no frame-rate loss in the headset (target 90 fps).
- The game's logic rate and guest state never change.
- Off unless a `textures` package is enabled; `vr.texturePacks` off → no lookups, no loader thread.
- Any mismatch or error → the original texture, never a crash or a wrong image.
- Scales: Native, ×2, ×4, ×8, ×16, ×32; precedence texture > class > global; global default ×4; max side
  2048 / 4096 (default) / 8192 / 16384.
- Step down, never skip: a scale that can't be reached gets the next lower one (×32 → ×16 → … → ×2), for max side,
  device limit 16384, model failure and consistency failure (< 22 dB); a conventional filter only after ×2 failed
  with the class's model. Exception: the rebuilt-archive per-archive budget may still leave textures native.
- Pack formats: BC1/BC2/BC3 → BC7 (same sRGB-ness); BC4 → BC4, BC5 → BC5 (same signedness); RGBA8 → BC7 of the
  same colour space, RGBA8 for classes UI/Lettering; anything else → no entry.
- Never modify the disc dump; never reset, discard, commit or push in either repo without the user's explicit OK.
  **Every "Commit" step below means: ask the user, commit only on a yes.**
- No game or headset launches without the user's go-ahead; headset tests at `WWHD_VR_FRAME_RATE=full`, report
  average fps and % at 90; < 35 fps average → stop and close the game.
- Recomp builds and tests in `build/game` (Ninja, Release): `cmake --build build/game --target <t>` then
  `ctest --test-dir build/game -R <name> --output-on-failure`. Game + launcher: `--target wwhd --target wwhd-vr-launcher`.
- Studio: `cargo test -p wwhd_tex` / `cargo build --release` from `TS:`; real-disc tests with
  `WWHD_DISC=ZeldaWindWakerRecomp\game_disc`.
- Match each file's existing style (the recomp's dense comment-explained C++; the Studio's doc-commented Rust).

## Review Focus

1. **Texture rewritten by the game after a replacement was made** (e.g. a texture whose bytes the game patches):
   the next draw must bind the original. Test in Task 6 (`bind rule` with a stale key).
2. **Two enabled packs with the same key, or a pack index listing a missing / truncated DDS:** first pack wins,
   bad entry is marked bad once and the original keeps drawing; the game never stops. Tests in Tasks 2 and 4.
3. **A texture at or above max side, or a non-power-of-two texture (e.g. 320×240):** ×2 when it fits, Native only
   when even ×2 exceeds max side; plan reason says why. Tests in Task 8.
4. **×32 on a 64 px texture with a ×4-only model, model failing at ×32:** passes ×4·×4·×2 (Lanczos ½ for the ×2),
   then ×16 with the model on failure, never straight to Lanczos. Tests in Task 9.
5. **Units that fetch texels / query size or LOD / use explicit SAMPLE_L, and artistic-mip textures:** keep the
   original. Tests in Tasks 3 and 6.

---

## Part A — Recomp (VR:)

### Task 1: `textures` package kind in the mod manager

**Files:**
- Modify: `VR:runtime/src/mods/packages.cpp:164` (kind check), `:254` region (kind-specific parsing), startup
  activation near `:758`, `list()` `restart_required` at `:981`, the update/remove/refresh messages that list
  restart-required kinds (`:999`, `:1004`, `:1100`, `:582`)
- Modify: `VR:runtime/src/mods/packages.h` (Manifest field, new function, `kManagerVersion` → `"1.7.0"` with a
  "1.7: kind textures" note)
- Test: `VR:runtime/tools/mod_package_test.cpp`, `VR:CMakeLists.txt` (`add_test(NAME mod_textures_startup COMMAND mod_package_test --textures-startup)`)

**Interfaces:**
- Produces: `std::vector<std::filesystem::path> packages::texture_roots();` — enabled `textures` packages'
  `<package>/<textures_dir>` folders in startup load order, fixed at startup (like content activation).
- Produces: `Manifest::textures_dir` (`std::string`, relative).
- Produces: `void packages::note_texture_pack(const std::filesystem::path& root, const std::string& text);` — appends
  `text` to the status of the package owning `root` (shown in the Mods tab); used by Task 4 for index warnings
  (duplicate keys, skipped entries) and failures.

- [ ] **Step 1: Write the failing tests** in `mod_package_test.cpp`, following the `--content-startup` mode:
  - a manifest `{"format_version":1,"id":"tex.a","name":"A","version":"1.0.0","game_id":"wwhd-usa","kind":"textures","textures_dir":"textures"}`
    with a `textures/index.json` installs, lists with `kind=="textures"` and `restart_required==true`;
  - `textures_dir` missing, absolute, `..`, or a symlink → `problem`/install error ("Invalid textures directory");
  - two enabled packages `tex.a`, `tex.b` → after startup `texture_roots()` equals `{A/textures, B/textures}` in
    load order; disabled ones absent; status text `"texture pack"`;
  - `note_texture_pack(A/textures, "2 duplicate keys")` → `list()` status of `tex.a` contains that text.
- [ ] **Step 2: Run** `cmake --build build/game --target mod_package_test` then
  `ctest --test-dir build/game -R "mod_textures_startup|mod_packages" --output-on-failure` → FAIL ("Unsupported mod kind").
- [ ] **Step 3: Implement**: accept `"textures"`; parse `textures_dir` with the same checks as `content_dir`
  (`archive::relative_path`, no symlink parts, directory exists); treat `textures` like `content` for
  restart-required, no dependencies, and the "disable and restart before updating/removing" messages; at startup
  record the active roots for `texture_roots()`.
- [ ] **Step 4: Run** the same ctest → PASS; also `-R mod_` → all PASS.
- [ ] **Step 5: Commit** (ask first): `mods: textures package kind (texture_roots)`.

### Task 2: `texture_pack_core.h` — pure rules (index, formats, DDS check, LODs)

**Files:**
- Create: `VR:runtime/src/gfx/texture_pack_core.h`
- Test: `VR:runtime/tools/texture_pack_test.cpp`; `VR:CMakeLists.txt` (`add_executable(texture_pack_test …)`,
  include `runtime/src`, `-UNDEBUG`, `add_test(NAME texture_pack COMMAND texture_pack_test)`, as `artistic_mips_test`)

**Interfaces (namespace `gfx::texture_pack`):**
- `struct Key { uint32_t format, width, height; uint64_t hash; bool operator==(const Key&) const = default; };` + `struct KeyHash`.
- `struct Entry { std::filesystem::path file; uint32_t scale; uint32_t dxgi; bool bad = false; };`
- `struct Index { std::unordered_map<Key, Entry, KeyHash> entries; std::vector<std::string> warnings; };`
- `bool parse_index(std::string_view json, const std::filesystem::path& root, Index& into, std::string& error);`
  — merges into `into`; a key already present is kept (first pack wins) and a warning added; an entry with a bad
  field is skipped with a warning; a non-object document / wrong `format_version` → `false`.
- `uint32_t allowed_dxgi(uint32_t gx2Format, uint32_t dxgi);` → `dxgi` if the pair is in the spec's table, else 0.
  GX2 codes: BC1 0x31/0x431, BC2 0x32/0x432, BC3 0x33/0x433, BC4 0x34/0x234, BC5 0x35/0x235, RGBA8 0x1A/0x41A.
  DXGI: BC7 98/99(sRGB), BC4 80/81(SNORM), BC5 83/84(SNORM), RGBA8 28/29(sRGB).
- `struct DdsLevels { uint32_t width, height, dxgi; std::vector<std::span<const uint8_t>> levels; };`
- `bool check_dds(std::span<const uint8_t> file, const Key& key, const Entry& e, uint32_t maxDim, DdsLevels& out, std::string& error);`
  — "DDS " magic, DX10 header, dxgi == `e.dxgi` and `allowed_dxgi(key.format, dxgi)`, scale ∈ {2,4,8,16,32},
  width == key.width·scale and height == key.height·scale, both ≤ maxDim, mip count ≥ 1 and ≤ 15, file size ≥ sum
  of level sizes (block-compressed: ceil(w/4)·ceil(h/4)·bytesPerBlock).
- `struct LodRange { float minLod, maxLod; };`
  `LodRange replacement_lods(float minLod, float maxLod, bool mipFilter, uint32_t offset);` — no mip filter →
  unchanged; else `maxLod + offset`, `minLod > 0 ? minLod + offset : 0`.
- `struct BindFacts { bool hasReplacement, keyCurrent, gpuWritten, boundAsTarget, needsGuestLevels, artisticMips; };`
  `bool may_bind(const BindFacts&);` — true only for `hasReplacement && keyCurrent && !gpuWritten && !boundAsTarget && !needsGuestLevels && !artisticMips`.
- JSON via the mod manager's parser (`mods/mod_json.h`, `json::parse`).

- [ ] **Step 1: Write failing tests** (plain `assert`-style `main` like `artistic_mips_test.cpp`):
  - `parse_index` of two entries → both found by `Key`; hash parsed from 16 hex digits; `file` resolved under root;
  - second index with an existing key → kept from first, `warnings.size()==1`;
  - entry with scale 3, width 0, missing file field, hash of 15 digits → skipped, warning each; wrong
    `format_version` → `false`;
  - `allowed_dxgi(0x31,98)==98`, `allowed_dxgi(0x431,99)==99`, `allowed_dxgi(0x431,98)==0`, `allowed_dxgi(0x235,84)==84`,
    `allowed_dxgi(0x35,84)==0`, `allowed_dxgi(0x1A,28)==28`, `allowed_dxgi(0x19,28)==0`;
  - `check_dds` on an in-memory BC7 DDS 64×64 with 7 levels for key 16×16, scale 4 → true, 7 levels, level 0 size
    4096; the same with height 60, scale 5, dxgi 71, maxDim 32, truncated by one byte, 16 levels → false each;
  - `replacement_lods(0, 10, true, 2) == {0, 12}`; `(1, 10, true, 2) == {3, 12}`; `(0, 0, false, 5) == {0, 0}`;
  - `may_bind` true for the all-good facts, false when any one flag flips (six cases).
- [ ] **Step 2: Run** `cmake --build build/game --target texture_pack_test && ctest --test-dir build/game -R texture_pack --output-on-failure` → FAIL (missing header).
- [ ] **Step 3: Implement** the header (inline functions).
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** (ask first): `gfx: texture pack rules (index, formats, DDS check, LOD shift)`.

### Task 3: Analyzer flag `textureNeedsGuestLevels`

**Files:**
- Modify: `VR:runtime/third_party/cemu/Cafe/HW/Latte/LegacyShaderDecompiler/LatteDecompiler.h:170` (field beside
  `textureUsesDepthCompare`), `LatteDecompilerAnalyzer.cpp:325-352` (set it)
- Test: `VR:runtime/tools/shader_translation_test.cpp`

**Interfaces:**
- Produces: `bool LatteDecompilerShader::textureNeedsGuestLevels[LATTE_NUM_MAX_TEX_UNITS]{};` — true for a unit
  sampled with texel coordinates (`useTexelCoords`), `SAMPLE_L`, `SAMPLE_C_L`, or queried with
  `GET_TEXTURE_RESINFO` / `GET_COMP_TEX_LOD`. Mark the change with a `// WWHD:` comment like the neighbouring ones.

- [ ] **Step 1: Write failing tests** in `shader_translation_test.cpp` using its existing shader-building helpers:
  one pixel shader per opcode (SAMPLE normalized, SAMPLE unnormalized, LD, SAMPLE_L, SAMPLE_LZ, RESINFO,
  GET_COMP_TEX_LOD) on unit 0 → flag true for unnormalized SAMPLE, LD, SAMPLE_L, RESINFO, GET_COMP_TEX_LOD; false
  for normalized SAMPLE and SAMPLE_LZ.
- [ ] **Step 2: Run** `cmake --build build/game --target shader_translation_test && ctest --test-dir build/game -R shader_translation --output-on-failure` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** → PASS; the shader identity / disk cache key does not need it (analysis output, not source).
  Confirm no shader-cache test changes: `ctest --test-dir build/game -R "shader" --output-on-failure` → PASS.
- [ ] **Step 5: Commit** (ask first): `shader analysis: units that need the guest's own levels`.

### Task 4: `texture_pack.{h,cpp}` — registry and loader thread

**Files:**
- Create: `VR:runtime/src/gfx/texture_pack.h`, `VR:runtime/src/gfx/texture_pack.cpp` (add to the runtime sources
  in `CMakeLists.txt` beside the other `runtime/src/gfx/*.cpp`)
- Test: `VR:runtime/tools/texture_pack_test.cpp` (link `texture_pack.cpp` into the test)

**Interfaces (namespace `gfx::texture_pack`):**
- Consumes: Task 2's `Key`, `Entry`, `Index`, `parse_index`, `check_dds`.
- `bool load(const std::vector<std::filesystem::path>& roots, uint32_t maxDim);` — reads each `index.json`;
  a failing package is logged (`[texture pack]`) and skipped; warnings and failures also go to
  `packages::note_texture_pack` (Task 1); returns whether any entry exists. Starts the loader thread only then.
- `WWHD_TEXTURE_PACK_LOG=1`: one log line per request, load, bad entry and release, and per guest upload whose
  format and size match an entry but whose hash does not ("near miss").
- `bool active();` `const Entry* lookup(const Key&);`
- `void request(const Key&);` — queue once per key (no duplicates in flight); ignored for `bad` entries.
- `struct Loaded { Key key; uint32_t width, height, dxgi, scale; std::vector<uint8_t> file; std::vector<std::span<const uint8_t>> levels; };`
- `std::vector<Loaded> take_ready(uint64_t maxBytes);` — finished loads up to `maxBytes` total (at least one).
- `void mark_bad(const Key&, const std::string& why);` — logged once per key.
- `void shutdown();`
- Loader holds ≤ 256 MiB of finished data (`kMaxReadyBytes`); beyond that it waits.
- Stats: `struct Stats { uint64_t entries, requested, loaded, bad, readyBytes; }; Stats stats();`

- [ ] **Step 1: Write failing tests** (temp folder under the test's working dir, files written by the test):
  - `load` of two roots with overlapping keys → `lookup` finds the first root's file;
  - `request` + `take_ready` (poll up to 2 s) returns one `Loaded` whose levels match the written DDS;
  - `request` of a key whose DDS is truncated → nothing ready, `stats().bad == 1`, a second `request` does nothing;
  - `request` twice before completion → one load (`stats().requested == 1`);
  - `load({})` → `active()==false`, no thread (`stats().entries == 0`).
- [ ] **Step 2: Run** `ctest --test-dir build/game -R texture_pack --output-on-failure` → FAIL.
- [ ] **Step 3: Implement** (one `std::thread`, `std::mutex` + `std::condition_variable` queue; file read with
  `std::ifstream` binary; `check_dds` on the bytes; spans point into `Loaded::file`).
- [ ] **Step 4: Run** → PASS.
- [ ] **Step 5: Commit** (ask first): `gfx: texture pack registry and loader thread`.

### Task 5: Detection, creation, lifetime and budget (`surfaces.cpp`)

**Files:**
- Modify: `VR:runtime/src/gfx/vulkan/backend.h` (`Surface`: `uint64_t level0Hash=0; std::shared_ptr<Surface> replacement; gfx::texture_pack::Key replacementKey{}; uint32_t lodOffset=0; uint64_t replacementUsedFrame=0;`)
- Modify: `VR:runtime/src/gfx/vulkan/surfaces.cpp` — `upload_surface` (`:1909-2071`), new
  `texture_pack_step()` beside `texture_budget_step` (`:1881`), `destroy_surface_image`/surface removal paths
  (`invalidate_surface_cache`, `ss_reset_surfaces`) to drop replacements, stats line
- Modify: `VR:runtime/src/gfx/vulkan/backend.cpp:3845` (`swap()`: call `texture_pack_step()` after
  `texture_budget_step()`), renderer start (call `texture_pack::load(packages::texture_roots(), maxImageDimension2D)`
  when `vr::config().texturePacks`)
- Modify: `VR:runtime/src/vr/vr_config.h` — `bool texturePacks = true;` (`vr.texturePacks`, in `kBools`) and
  `float texturePackBudgetMB = 0;` (`vr.texturePackBudgetMB`, 0 = automatic, in `kFloats`), with comments in the
  file's style; covered by the existing vr_config tests if present (add one assertion each)

**Interfaces:**
- Consumes: Task 4 (`active`, `lookup`, `request`, `take_ready`, `mark_bad`), Task 2 (`Key`).
- Produces: `Surface* replacement_for(Surface* guest);` (surfaces.cpp, declared in backend.h) — the replacement if
  created and `replacementKey == current key`, else `nullptr`; sets `replacementUsedFrame = R.frame`.
- Produces: `void texture_pack_step();`

Behaviour (spec §3, §4, §6):
- `upload_surface`: keep the level-0 `content_hash` term as `s->level0Hash` (computed in the existing loop, no new
  hashing). After a real upload, if `texture_pack::active()` and the surface is a candidate (not depth, not
  `gpuWritten`, `imageType 2D`, `slices==1`, `arrayLayers==1`): key `{s->format, s->width, s->height, s->level0Hash}`;
  key differs from `replacementKey` → drop the replacement; key in the map and no replacement → `request(key)`.
- `texture_pack_step()`: `take_ready(64 MiB)`; for each loaded key find the waiting surfaces (a small
  `std::unordered_multimap<Key, Surface*>` of requesters, cleared when a surface is destroyed); discard if the key
  changed; else create a `Surface` (`width/height` = guest × scale, `mips` = levels, `fmt` from the dxgi →
  `VK_FORMAT_BC7_UNORM/SRGB_BLOCK`, `BC4_UNORM/SNORM`, `BC5_UNORM/SNORM`, `R8G8B8A8_UNORM/SRGB`, `sx=sy=scale`,
  `lodOffset=log2(scale)`), `create_surface_image(c, false)`, upload each level through `texture_staging_slice` /
  a staging buffer as `upload_surface` does, transition to shader read. Image creation failure → release LRU
  replacements, retry once, else `mark_bad`.
- Budget: `vr.texturePackBudgetMB` or, at 0, min(heapBudget/2, heapBudget − heapUsage) read once after the VR
  session starts, ≥ 1 GiB. Over budget → `texture_budget::select` over replacements with `pin = 300` frames;
  release = destroy image, keep nothing (re-requested on next upload check of the guest).
- The guest texture budget's eviction leaves the replacement alone; destroying the guest destroys it.
- Stats line (where `g_stat_uploads` is printed): `texpack hits %llu pending %llu VRAM %.0f/%.0f MiB guest-levels %llu`;
  the same VRAM used / budget figure in the F1 overlay's renderer statistics (find the section that shows the
  texture budget or upload stats: `grep -rn "texture budget\|g_stat_uploads" VR:runtime/src/gfx`).
- Near-miss logging (Task 4's `WWHD_TEXTURE_PACK_LOG`): needs a set of (format, width, height) present in the
  index, exposed as `bool texture_pack::has_shape(uint32_t format, uint32_t w, uint32_t h)` (add to Task 4's
  interface and test: true for an indexed shape, false otherwise).

- [ ] **Step 1: Write failing tests** in `texture_pack_test.cpp` for the pure parts this task adds to
  `texture_pack_core.h`: `uint32_t lod_offset(uint32_t scale)` (2→1 … 32→5, others → 0) and
  `uint64_t default_budget(uint64_t heapBudget, uint64_t heapUsage)` (8 GiB/1 GiB → 4 GiB; 8 GiB/6 GiB → 2 GiB;
  2 GiB/1.9 GiB → 1 GiB).
- [ ] **Step 2: Run** → FAIL.
- [ ] **Step 3: Implement** the two helpers, then the surfaces/backend/vr_config changes above.
- [ ] **Step 4: Build and run all tests:** `cmake --build build/game --target wwhd --target wwhd-vr-launcher` →
  builds with no new warnings; `ctest --test-dir build/game --output-on-failure` → all PASS (renderer smoke tests
  included).
- [ ] **Step 5: Commit** (ask first): `vulkan: build replacement images for texture packs`.

### Task 6: Binding and sampler offset (`draw.cpp`)

**Files:**
- Modify: `VR:runtime/src/gfx/vulkan/draw.cpp` — `resolve_texture` (`:2575`), `resolve_sampler` (`:2630`),
  `sampler(...)` (`:1700-1800`: new parameter `uint32_t lodOffset`, applied via `replacement_lods`, appended to the
  cache key and bypassing the memo when non-zero, as `extraBias` does)
- Modify: `VR:runtime/src/gfx/texture_pack_core.h` only if a helper is missing (none expected)

**Interfaces:**
- Consumes: `replacement_for` (Task 5), `may_bind`, `replacement_lods` (Task 2),
  `textureNeedsGuestLevels` (Task 3).

Behaviour: in `resolve_texture`, after the guest surface `s` is final (after the D-sweep stand-in and AO
handling), build `BindFacts{ r != nullptr, true, s->gpuWritten, aliases-as-target (the same test the caller's
`aliases_target` uses), sh->dec->textureNeedsGuestLevels[unit], s->artisticMips }` with `r = replacement_for(s)`;
`may_bind` → return `r`. The caller's existing `s->sx/sy` → `uf_texNScale` and views then work unchanged.
`resolve_sampler` passes `s->lodOffset` (0 for guest surfaces). Count units refused for `needsGuestLevels` in the
stats line.

- [ ] **Step 1: Write failing tests**: extend `texture_pack_test.cpp` with the stale-key case (Review Focus 1):
  facts with `keyCurrent=false` → `may_bind` false. Then add a renderer smoke case in `VR:runtime/src/gfx/vulkan/smoke.cpp`'s
  existing texture section, if it has one that uploads a guest texture: a pack with that texture's key at ×2 → the
  sampled image's extent is twice the guest's; a write to the guest bytes → the next resolve returns the guest
  surface. (If smoke.cpp has no guest-texture upload case, record that in the task notes and rely on the desktop
  check in Task 13.)
- [ ] **Step 2: Run** `ctest --test-dir build/game -R "texture_pack|smoke" --output-on-failure` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** the full ctest → PASS; build `wwhd` + `wwhd-vr-launcher` → OK.
- [ ] **Step 5: Commit** (ask first): `vulkan: bind texture pack replacements (LOD-shifted samplers)`.

## Part B — Texture Studio (TS:)

### Task 7: XXH3 key and DDS writer (`runtime_pack.rs` core)

**Files:**
- Create: `TS:crates/wwhd_tex/src/runtime_pack.rs` (+ `pub mod runtime_pack;` in `lib.rs`)
- Modify: `TS:crates/wwhd_tex/Cargo.toml` — `xxhash-rust = { version = "0.8", features = ["xxh3"] }`; record it in
  `TS:docs/dependencies.md` (licence BSL-1.0, crates.io)
- Test: in-module tests; `TS:crates/wwhd_tex/tests/real_disc.rs` (new test `runtime_keys_fixture`)

**Interfaces:**
- `pub fn key_hash(level0: &[u8]) -> u64` (xxh3_64, seed 0).
- `pub fn level0_bytes<'a>(data: &'a [u8], surface: &Gx2Surface) -> &'a [u8]` — first `surface.image_size` bytes.
- `pub enum PackFormat { Bc7 { srgb: bool }, Bc4 { snorm: bool }, Bc5 { snorm: bool }, Rgba8 { srgb: bool } }`
  with `fn dxgi(&self) -> u32` (values as Global Constraints / Task 2).
- `pub fn pack_format(gx2_format: u32, class: Class) -> Option<PackFormat>` — the spec table.
- `pub fn write_dds(width: u32, height: u32, fmt: PackFormat, levels_rgba: &[Vec<u8>]) -> Result<Vec<u8>>` —
  DX10 header, encodes each level with `image_dds` (BC7 quality Slow); level sizes as Task 2's `check_dds`.

- [ ] **Step 1: Write failing tests:** `key_hash(&[]) == 0x2D06800538D394C2`; `key_hash(b"wwhd")` equals the
  constant printed once by the implementation and pasted into **both** this test and a matching assertion added to
  `VR:runtime/tools/texture_pack_test.cpp` (`XXH3_64bits("wwhd",4)`), so both sides are pinned to one value;
  `pack_format` for every row of the table (BC1 SRGB 0x431 → `Bc7{srgb:true}`, 0x235 → `Bc5{snorm:true}`,
  RGBA8 + `Class::Ui` → `Rgba8`, RGBA8 + other class → `Bc7`, 0x19 → `None`); `write_dds` of 64×64 + mips → parses
  back with `ddsfile` (or the header read by hand) to the same width, height, dxgi, mip count, and total size.
  Real-disc `runtime_keys_fixture`: for 20 fixed textures (first 20 decodable in inventory order), the key hash of
  `level0_bytes` equals `key_hash` of the FTEX image slice re-read from the archive (guards the slicing).
- [ ] **Step 2: Run** `cargo test -p wwhd_tex runtime_pack` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p wwhd_tex` and, with `WWHD_DISC` set, `cargo test -p wwhd_tex --test real_disc runtime_keys_fixture` → PASS; then the recomp's `texture_pack` ctest → PASS with the pasted constant.
- [ ] **Step 5: Commit** (ask first, Studio repo has no commits yet).

### Task 8: Resolution policy (global, per class, per texture, step-down sizing)

**Files:**
- Modify: `TS:crates/wwhd_tex/src/project.rs` (`Settings`), `TS:crates/wwhd_tex/src/policy.rs` (`plan`, `plan_all`)

**Interfaces:**
- `pub const SCALES: [u32; 5] = [2, 4, 8, 16, 32];`
- `pub enum ScaleChoice { Global, Native, X(u32) }` (serde; `X` only from `SCALES`).
- `Settings`: `scale: u32` (global, default 4, values 1 or `SCALES`), `max_dim: u32` (2048/4096/8192/16384,
  default 4096), new `class_scales: Vec<(Class, ScaleChoice)>` with `default_class_scales()` = `Native` for
  `Class::Ramp` (the class whose backend is `Native` in `default_class_backends`), `Global` for every other class
  (unsupported formats stay native through `plan`'s existing `decodable` check, not a class). Old `settings.json` without
  `class_scales` loads with the default (`#[serde(default)]`).
- `Review.scale: Option<u32>` accepts `SCALES` and 1.
- `pub fn wanted_scale(t: &TextureRecord, class: Class, s: &Settings, review: Option<&Review>) -> (u32, &'static str)`
  — precedence texture > class > global; the str is "texture"/"class"/"global".
- `pub fn fit_scale(want: u32, max_side: u32, max_dim: u32) -> u32` — halve until `max_side·scale ≤ max_dim`
  (with `max_dim` capped at 16384); returns 1 only when ×2 does not fit.
- `Plan.reason` strings: `"<class>: x<want> (<source>)"`, plus `" → x<n>: max side <max_dim>"` on a step down, or
  `"native: <reason>"`.
- `plan_all`: the per-archive budget halves the largest textures and may reach 1 (the documented exception);
  reason `" → native: archive heap budget"`.
- Remove the "≥ 2048 px stays native" rule.

- [ ] **Step 1: Write failing tests** in `policy.rs`:
  - precedence: global 4, class X(8), review Some(16) → 16 "texture"; review None → 8 "class"; class Global → 4 "global"; class Native → plan native;
  - `fit_scale(32, 128, 4096)==32`, `(32, 512, 4096)==8`, `(4, 2048, 4096)==2`, `(4, 4096, 4096)==1`,
    `(32, 64, 16384)==32`, `(32, 1024, 65536)==16` (capped at 16384), `(16, 320, 4096)==8`
    (non-power-of-two side, Review Focus 3);
  - a 2048 px texture with default settings plans ×2 (old rule gone), reason contains "max side 4096";
  - an old `settings.json` without `class_scales` deserialises with the defaults;
  - `plan_all` with a tiny archive budget leaves a texture native with reason "archive heap budget" (exception kept).
- [ ] **Step 2: Run** `cargo test -p wwhd_tex policy` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p wwhd_tex` → PASS.
- [ ] **Step 5: Commit** (ask first).

### Task 9: Multi-pass upscaling and step-down on failure

**Files:**
- Modify: `TS:crates/wwhd_tex/src/upscale.rs` (`native_scales`, new pass planner), `TS:crates/wwhd_tex/src/pipeline.rs`
  (`upscale`, `run_with_recovery:301`, `consistency:182`, the `.json` side record)

**Interfaces:**
- `pub fn pass_plan(native: &[u32], want: u32) -> Vec<(u32, u32)>` — passes of (run scale, kept scale): greedily
  the largest native power-of-two scale ≤ remaining, ×4 preferred; when the remainder is 2 and the model has no ×2,
  `(4, 2)` (run ×4, Lanczos ½). Filters return `[(want, want)]` (one step, any power of two).
- `run_with_recovery` gains the step-down: on model error or consistency < 22 dB at scale `n`, retry the same
  backend at `n/2` while `n/2 ≥ 2`; only then the existing Lanczos-3 at the largest scale the plan allowed. The
  result's `.json` records `wanted`, `achieved`, and each step as `"x16 failed consistency (19.3 dB) → x8"`.
- Upscale output path stays `upscaled_path(backend, hash, achieved)`; the plan's export step reads the achieved
  scale from the `.json` (no fixed assumption that achieved == planned).
- Each pass pads (16 px wrap/clamp) and crops as today; consistency compares the final result with the original.

- [ ] **Step 1: Write failing tests:** `pass_plan(&[4], 32) == [(4,4),(4,4),(4,2)]`; `pass_plan(&[2,4], 8) == [(4,4),(2,2)]`;
  `pass_plan(&[2], 8) == [(2,2),(2,2),(2,2)]`; `pass_plan(&[4], 2) == [(4,2)]`; with a fake backend (test-only
  `BackendChoice::Custom` stub or a closure seam in `run_with_recovery`) that fails at ×32 and ×16 and succeeds at ×8
  → achieved 8, two recorded steps, no Lanczos (Review Focus 4); a stub that always fails → Lanczos at the planned
  scale, recorded.
- [ ] **Step 2: Run** `cargo test -p wwhd_tex upscale pipeline` → FAIL.
- [ ] **Step 3: Implement** (introduce the seam as a `fn`-pointer/closure parameter of an inner function so the real
  `run_with_recovery` stays the public entry).
- [ ] **Step 4: Run** `cargo test -p wwhd_tex` → PASS.
- [ ] **Step 5: Commit** (ask first).

### Task 10: Runtime pack export (pipeline, CLI, install)

**Files:**
- Modify: `TS:crates/wwhd_tex/src/runtime_pack.rs` (`export_runtime`), `TS:crates/wwhd_tex/src/pipeline.rs`
  (encode cache key + output format), `TS:crates/texture_studio/src/cli.rs` (`export-runtime`, install warning)

**Interfaces:**
- Consumes: Task 7 (`key_hash`, `level0_bytes`, `pack_format`, `write_dds`), Task 8 (`plan_all` without
  archive budget/exclusions for this export), Task 9 (achieved scale), existing `mips::chain`, `debug_tint`.
- `pub fn export_runtime(p: &Project, s: &Settings, inv: &Inventory, d: &Decisions, progress: &Progress) -> Result<RuntimeReport>`
  — one entry per unique key; DDS at `output/runtime/wwhd-texture-studio-tex-N/textures/<aa>/<hash16>.dds`;
  `index.json` per package (format of spec "Pack format", `generator` = `"ZeldaWWHD Texture Studio " + CARGO_PKG_VERSION`);
  `manifest.json` `{format_version:1,id:"wwhd-texture-studio-tex-N",name,version,game_id:"wwhd-usa",kind:"textures",textures_dir:"textures"}`;
  packages ≤ 4 GiB; textures with no `pack_format` or not 2D/1 slice are listed in the report, not exported.
  `debug_tint` applies as in the archive export. Report: entries, bytes, VRAM estimate, skipped with reasons;
  warning when the estimate > 8 GiB.
- CLI: `texture_studio --headless export-runtime --project DIR [--only …] [--debug-tint]`;
  `install --kind runtime|archives` (default archives, as today); installing runtime packs while archive packages
  are enabled in `profiles.json` (or the reverse) prints the spec's warning.

- [ ] **Step 1: Write failing tests:** `export_runtime` on a small synthetic inventory (2 textures sharing one key,
  1 RGBA8 UI, 1 unsupported format) in a temp project → 2 entries (shared key once), one `Rgba8` DDS, one skipped
  with reason; `index.json` parses and every `file` exists; a split test with the package limit lowered (internal
  parameter) → 2 packages with no key in both.
- [ ] **Step 2: Run** `cargo test -p wwhd_tex runtime_pack` → FAIL.
- [ ] **Step 3: Implement.**
- [ ] **Step 4: Run** `cargo test -p wwhd_tex`; then on the real project:
  `target/release/texture_studio.exe --headless export-runtime --project ZeldaWWHD_TextureStudio`
  → completes, 0 errors, report lists entries/bytes; spot-check: the pack's key for `Bgn.szs/Bgn.bfres/<first texture>`
  is the 16-hex-digit `key_hash` of its level-0 bytes (the recomp side is pinned by Task 7's shared constant).
- [ ] **Step 5: Commit** (ask first).

### Task 10b: Mods folder auto-detection (install)

**Files:** Create `TS:crates/wwhd_tex/src/mods_dir.rs` (+ `pub mod mods_dir;`), modify `TS:crates/texture_studio/src/cli.rs`
(`install`: `--mods-root` optional), `TS:crates/wwhd_tex/src/project.rs` (`Settings.mods_root: Option<PathBuf>`,
`Settings.game_exe: Option<PathBuf>`, both `#[serde(default)]`).

**Interfaces:**
- `pub enum ModsRule { Explicit, ManagerEnv, UserDirEnv, Portable, Platform }`
- `pub struct Env { pub manager_dir: Option<OsString>, pub user_dir: Option<OsString>, pub appdata: Option<OsString>, pub xdg_config: Option<OsString>, pub home: Option<OsString> }` with `Env::from_process()`.
- `pub fn detect(explicit: Option<&Path>, game_exe: Option<&Path>, env: &Env) -> (PathBuf, ModsRule)` — order as the
  spec's "Mods folder auto-detection"; pure (no process env inside), so it's testable.

- [ ] **Step 1: Write failing tests:** each rule wins over the ones after it; portable needs `portable.txt` next
  to the exe (temp dir) and gives `<parent>/user/ModManager`; Windows platform rule `APPDATA=E:\Cache\Roaming` →
  `E:\Cache\Roaming\WWHD\ModManager`; Linux XDG set / unset (`#[cfg]`-gated per OS).
- [ ] **Step 2: Run** `cargo test -p wwhd_tex mods_dir` → FAIL.
- [ ] **Step 3: Implement**; CLI `install` without `--mods-root` uses `detect`, prints `mods folder: <path> (<rule>)`,
  errors with that path if it doesn't exist.
- [ ] **Step 4: Run** `cargo test -p wwhd_tex` → PASS.
- [ ] **Step 5: Commit.** (Task 11's Export/Install view shows the detected folder and rule, a Settings override
  field, and a create-folder confirmation.)

### Task 11: GUI — resolution policy, per-class scale, estimates, runtime export

**Files:**
- Modify: `TS:crates/texture_studio/src/app.rs` (Settings view, Assets detail override, Compare models scales,
  Dashboard, Export/Install view)

Behaviour:
- Settings → *Resolution policy*: Scale (Native, ×2…×32), Max side (2048/4096/8192/16384).
- Settings → *Model per class*: a **Scale** combo next to each class's Model (Global, Native, ×2…×32).
- Assets: scale override combo offers ×2…×32; the plan line shows `Plan.reason` (with step-downs).
- Compare models: a scale selector (×2…×32) for the comparison run.
- Dashboard and Settings: per class and total texels, disk size and VRAM estimate at the current policy
  (`policy::surface_bytes` on the pack format), updating when the policy changes.
- Export/Install: two buttons, *Export rebuilt archives* and *Export runtime pack*, each with its own install; the
  "use one or the other" note and the > 8 GiB warning.

- [ ] **Step 1:** `cargo build --release` → OK, 0 warnings.
- [ ] **Step 2:** Run the GUI, open the project, set class Environment → ×8, check the Assets plan of a 512 px
  environment texture reads "×8 (class)" and a 1024 px one "x8 → x4: max side 4096"; set the global to ×32 and
  see the estimate rise; screenshot for the user.
- [ ] **Step 3: Commit** (ask first).

## Part C — Docs and verification

### Task 12: Docs

**Files:** `TS:README.md` (status rows), `TS:docs/user-guide.md` (resolution policy, runtime pack, install kinds),
`TS:docs/design.md` (integration status), `TS:docs/runtime-adapter.md` (implemented note),
`VR:docs/mod-manager.md` (kind `textures`, `textures_dir`, restart, warning on duplicate keys), and the recomp's
settings documentation file that lists `vr.*` keys (find with `grep -rl "vr.frameRate" VR:docs`).

- [ ] **Step 1:** Write the sections; every setting name and default matches the code.
- [ ] **Step 2: Commit** (ask first).

### Task 13: Full verification, then game checks (each launch asked first)

- [ ] **Step 1:** `ctest --test-dir build/game --output-on-failure` → all PASS; `cargo test` (with `WWHD_DISC`) → all PASS; both release builds 0 new warnings.
- [ ] **Step 2 (ask):** Desktop: export a tinted runtime pack, install into `ZeldaWWHD_TextureStudio\game_test\ModManager`,
  `WWHD_MOD_MANAGER_DIR=… WWHD_TEXTURE_PACK_LOG=1 build/game/wwhd.exe --game <game_disc> --no-vr`; title scene,
  Outset, Dragon Roost: replaced textures magenta, others unchanged; log shows hits and no bad entries.
- [ ] **Step 3 (ask):** Headset, Capped 90 (`WWHD_VR_FRAME_RATE=full`), untinted pack: same route; report average fps,
  % at 90, texture-pack VRAM / budget, pops seen; < 35 fps → stop and close.
- [ ] **Step 4:** Update memory (`wwhd-texture-studio.md`) with what was verified.
