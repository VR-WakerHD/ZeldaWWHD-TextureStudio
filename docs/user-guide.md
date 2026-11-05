# User guide

## Folders

- **Disc folder**: your extracted Wind Waker HD dump (`code/`, `content/`, `meta/`). Only read, never written.
- **Project folder**: everything the app makes. Expect 5–10 GB for a full ×4 run, and more for the runtime pack at high scales (about 12.7 GB on disc for the author's full export).

```
<project>/
  settings.json                         all settings (also editable in the GUI)
  source_inventory/inventory.json       every file and texture found, with hashes and gate results
  masters/<aa>/<key>.png                lossless level-0 image of each unique texture
  masters/thumbs2/...                   thumbnails (channel-mapped for display)
  cache/upscaled/<model>/<aa>/<key>_x<scale>.png (+ .json: model used, time, consistency, notes)
  manifests/review.json                 approvals, rejections, per-texture model/scale overrides, ratings
  manifests/classes.json                manual classifications
  manifests/replacement_manifest.json   every exported replacement and what it replaced
  output/packages/wwhd-texture-studio-N/{manifest.json,content/Common/...}      rebuilt archives
  output/runtime/wwhd-texture-studio-tex-N/{manifest.json,textures/index.json,textures/<aa>/*.dds}   runtime pack
  reports/export_report.json, runtime_export_report.json, compare_x4.png
  logs/studio.log
```

Identical textures that appear in several archives share one master and one upscale; each archive still gets
its own rebuilt copy.

## GUI

- **Top bar**: project and disc folders, **▶ Process everything**, the individual steps, pause/cancel and
  progress.
- **Dashboard**: counts and a per-class table with the model used and texture memory before/after.
  The top bar also checks that the disc folder looks right.
- **Assets**: thumbnail grid with search, class and status filters. Border colour: blue = will be upscaled,
  grey = stays native, green = approved, red = rejected. The right panel shows metadata, class (changeable),
  the plan, approve/reject/rating, scale override, *Reprocess with this model*, and a comparison
  (side by side, slider, alpha, normal map) with zoom. Batch buttons apply to everything shown.
- **Compare models**: runs the ticked models on the selected texture and shows them side by side; clicking a
  result makes that model the choice for this texture.
- **Enhance**: post-upscale sharpening, detail recovery and colour (see `docs/enhancement.md`). Left: the project's
  profile (Original, Enhanced, **Enhanced & Extra Vibrant** by default, Ultra Sharp, Ultra Sharp +, Ultra Sharp & Ultra Vibrant, or your own), duplicate / reset / delete / save / load
  of custom profiles, and every parameter. Right: the texture selected in Assets, with its own profile choice,
  suggested models for its class, and a comparison viewer (split with a movable divider, side by side, or variant
  grids of stages, profiles, sharpening and colour strengths, or models). Wheel zooms, drag pans, **Fit** and
  **100 %**. *As exported* shows the real BC encode. Changing a profile never re-upscales; the next export
  re-encodes.
- **Settings**: at the top, **Upscaling models**: which AI upscalers are installed, and **Install Upscaling Models**,
  which downloads the missing ones (Real-ESRGAN, waifu2x, Real-CUGAN, RealSR) from their official GitHub releases,
  checks each against its pinned SHA-256 and unpacks it into the tools folder (progress and Cancel in Queue). Then: scan scope, resolution policy (global scale, max side, "keep native below", and a scale per class
  next to the model per class), GPU/tile/batch, tools folder, the recomp's mods folder and game executable,
  package and memory limits, "only archives containing" (to try one area first), debug tint. A table shows what
  the runtime pack would hold at the current policy (replacements, texels, disk, texture memory).
- **Queue & log**: progress, rate, time left, pause/resume/cancel, full log.
- **Export / install**: the two ways to deliver the upscales, the estimates, **Export runtime pack** and
  **Export rebuilt archives**, and **Install runtime pack** / **Install rebuilt archives** into the recomp's
  `ModManager` folder (shown with how it was found). Use one kind or the other; installing one leaves the other
  kind's packages alone and warns when both are switched on.
- **Diagnostics**: formats found, textures that cannot be rewritten and why, scan errors, export problems.

## How a texture is processed

1. **Policy** (Settings → Resolution policy). Scales are Native, ×2, ×4, ×8, ×16 and ×32. A texture's scale comes
   from, in this order, its own setting (Assets → scale override), its class's setting (Settings → Models per
   class → Scale) and the global scale (default ×4). **Max side** (2048, 4096 (default), 8192 or 16384 px) limits
   the output's longest side; for a runtime pack played in VR use 4096 until the recomp spreads large uploads over
   frames (a larger replacement is made in one frame when the game first draws it, and Settings and Export warn).
   A scale that would pass it steps down one factor at a time (×32 → ×16 → … → ×2),
   never skipping; a texture stays native only when even ×2 would exceed the max side. The same stepping applies
   when a model fails at a large scale or its result fails the consistency check (below 22 dB): the next lower
   scale is tried, and a conventional filter is used only after ×2 failed with the class's model. Textures smaller
   than *keep native below* (16 px by default), lighting ramps, unsupported or gate-failed textures and rejected
   textures stay native. The plan in Assets says why for each texture.
   For the **rebuilt archives** there is a further limit: per archive, the largest textures are halved until the
   archive stays within **2× its original decompressed size** (the game's heaps are sized for the originals),
   +96 MiB of texture data and 120 MiB on disc. Excluded by default (verified to crash when enlarged):
   `Common/Misc/Misc.szs`, `Common/Object/Tlogo.szs`, `Common/Particle/Particle.szs`,
   `Common/ProgramTexture/ProgramTexture.szs` and everything in `Common/Pack/`. Both are in Settings → Packages
   and memory budget. The **runtime pack** has neither limit.
2. **Upscale**: colour classes use Real-ESRGAN x4plus-anime by default, UI and lettering Lanczos-3, masks
   Mitchell, normal maps Catmull-Rom on vectors (never AI). Colour goes to the model with a 16 px wrapped
   border (clamped for UI) so tiling textures stay seamless; alpha is resized separately. Each AI result is
   shrunk back and compared with the source; below 22 dB it is discarded and Lanczos-3 is used (noted).
   A failing model run is retried with a smaller tile, then image by image, then with Lanczos-3.
3. **Enhance** (both exports): the active profile is applied to the cached upscale in memory. Alpha, data textures
   (normal maps, masks, ramps, one/two-channel formats) and Unknown-class textures are left exactly as they are.
   **Export (rebuilt archives)**: mip chain (sRGB-correct, alpha coverage kept for cut-outs, normals renormalised), encode to the
   original GX2 format, tile, write into the archive, rebuild SARC/Yaz0, reopen and compare everything. Each
   texture must decode back at ≥ 24 dB of the intended image or the original is kept.

4. **Export (runtime pack)**: one entry per unique texture, keyed by the GX2 format, the size and the XXH3-64 hash
   of the level-0 data, so a texture used in several archives is stored once. Formats: BC1/BC2/BC3 → BC7 (same
   sRGB-ness), BC4 → BC4, BC5 → BC5 (same signedness), RGBA8 → BC7 of the same colour space (RGBA8 for the UI and
   lettering classes); anything else, and multi-slice or non-2D textures, get no entry and keep the original. Each
   DDS has `original mips + log2(scale)` levels. Packages are split at 4 GiB; textures not upscaled yet are
   skipped and listed in the report. The estimate of texture memory warns above 8 GiB; the game keeps only what
   its budget allows resident. Each entry also lists, in an optional `archives` field, the game files (paths below
   `content/`; a texture inside a `.pack` lists the pack) it comes from, so the recomp can load a replacement when
   the game opens one of them instead of when it first draws it.

## The runtime pack in the game

The recomp loads a runtime pack as a mod package of kind `textures` and binds the larger texture in place of the
game's at draw time. Switching the package on or off needs a restart. The settings `vr.texturePacks` (default on)
and `vr.texturePackBudgetMB` (default 0, automatic) are in the recomp's `settings.ini`; see its
`docs/VR_SETTINGS.md` and `docs/mod-manager.md`. **Status: implemented and tested with unit and smoke tests; not
yet verified in the game or the headset.** If the pack does nothing, check the package's line in the Mods tab
(`texture pack: N entries, …` or why it is off) and start the game with `WWHD_TEXTURE_PACK_LOG=1`.

## If the game crashes with the packages on

This section is about the rebuilt archives. The game halts with a message such as `stageRsrc != (0)`, `modelData != (0)` or a guest null-pointer read
when an enlarged archive does not fit the heap the game loads it into. Lower *max archive size vs original*
or add the archive to the exclusion list, export again (cached, so quick) and reinstall. Please report the
archive so it can be added to the defaults.

## Command line

`texture_studio --headless <command> --project DIR [options]`

| Command | |
|---|---|
| `scan --disc DIR [--scope content\|all\|PATH,PATH]` | inventory |
| `extract` | masters + thumbnails |
| `upscale [--only TEXT,TEXT]` | upscale planned textures (resumable) |
| `export [--only TEXT,TEXT]` | rebuild archives + content packages (`output/packages`) |
| `export-runtime [--only TEXT,TEXT] [--debug-tint]` | runtime texture packs (`output/runtime`) from the upscales made so far |
| `run [--disc DIR] [--only …]` | all of the above |
| `compare --names TEXT,TEXT [--backends all\|KEY,KEY] [--crop N]` | contact sheet of models in `reports/` |
| `enhance-bench [--per-class N \| --names TEXT,TEXT] [--backend KEY] [--scale N] [--crop N] [--max-side N]` | enhancement QA: sheets and metrics per texture in `reports/enhance_bench/` |
| `install-tools [--tools DIR]` | download the missing AI upscalers into the tools folder (official releases, SHA-256 checked) |
| `install [--mods-root DIR] [--kind archives\|runtime] [--no-enable]` | copy the packages of that kind (default `archives`) into the ModManager folder and enable them |
| `stats` | per-class counts and sizes |

`--debug-tint` / `--no-debug-tint` toggles the magenta diagnostics tint.

## Installing and testing with the recomp

`install` and the GUI find the recomp's ModManager folder the way the recomp does: `--mods-root` (or the folder set
in Settings), else `WWHD_MOD_MANAGER_DIR`, else `WWHD_USER_DIR/ModManager`, else portable mode (a `portable.txt`
next to the game executable: `<game folder's parent>/user/ModManager`), else the per-user config folder
(`%APPDATA%\WWHD\ModManager` on Windows, `~/.config/wwhd/ModManager` on Linux). The game executable is the one set
in Settings, or `build/game/wwhd(.exe)` beside the disc dump. The command line never creates the folder (start the
game once, or pass `--mods-root`); the GUI asks before creating it. Installing replaces earlier Texture Studio
packages of the same kind and switches the new ones on in the active profile.

To test without touching your normal setup:

```bash
texture_studio --headless install --project <project> --kind runtime --mods-root <project>/game_test/ModManager
WWHD_MOD_MANAGER_DIR=<project>/game_test/ModManager wwhd --game <disc> --no-vr
```

The packages appear in the game's Mods → Community list (rebuilt archives as content mods, the runtime pack as a
textures mod); turning them off restores the original textures after a restart.
