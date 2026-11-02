# User guide

## Folders

- **Disc folder**: your extracted Wind Waker HD dump (`code/`, `content/`, `meta/`). Only read, never written.
- **Project folder**: everything the app makes. Expect 5–10 GB for a full ×4 run.

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
  output/packages/wwhd-texture-studio-N/{manifest.json,content/Common/...}
  reports/export_report.json, compare_x4.png
  logs/studio.log
```

Identical textures that appear in several archives share one master and one upscale; each archive still gets
its own rebuilt copy.

## GUI

- **Top bar**: project and disc folders, **▶ Process everything**, the individual steps, pause/cancel and
  progress.
- **Dashboard**: counts and a per-class table with the model used and texture memory before/after.
- **Assets**: thumbnail grid with search, class and status filters. Border colour: blue = will be upscaled,
  grey = stays native, green = approved, red = rejected. The right panel shows metadata, class (changeable),
  the plan, approve/reject/rating, scale override, *Reprocess with this model*, and a comparison
  (side by side, slider, alpha, normal map) with zoom. Batch buttons apply to everything shown.
- **Compare models**: runs the ticked models on the selected texture and shows them side by side; clicking a
  result makes that model the choice for this texture.
- **Settings**: scan scope, resolution policy, model per class, GPU/tile/batch, tools folder, package and
  memory limits, "only archives containing" (to try one area first), debug tint.
- **Queue & log**: progress, rate, time left, pause/resume/cancel, full log.
- **Export / install**: target notes, memory budget with warnings, export, install into the recomp's
  `ModManager` folder.
- **Diagnostics**: formats found, textures that cannot be rewritten and why, scan errors, export problems.

## How a texture is processed

1. **Policy**: textures smaller than 16 px, at least 2048 px, lighting ramps, unsupported or gate-failed
   textures, and textures in excluded archives stay native. Others are scaled by ×4 (or ×2 where ×4 would
   exceed 4096 px). Per archive, the largest textures are halved until the archive stays within
   **2× its original decompressed size** (the game's heaps are sized for the originals), +96 MiB of texture
   data and 120 MiB on disc. Excluded by default (verified to crash when enlarged): `Common/Misc/Misc.szs`,
   `Common/Object/Tlogo.szs`, `Common/Particle/Particle.szs`, `Common/ProgramTexture/ProgramTexture.szs` and
   everything in `Common/Pack/`. Both are in Settings → Packages and memory budget.
2. **Upscale**: colour classes use Real-ESRGAN x4plus-anime by default, UI and lettering Lanczos-3, masks
   Mitchell, normal maps Catmull-Rom on vectors (never AI). Colour goes to the model with a 16 px wrapped
   border (clamped for UI) so tiling textures stay seamless; alpha is resized separately. Each AI result is
   shrunk back and compared with the source; below 22 dB it is discarded and Lanczos-3 is used (noted).
   A failing model run is retried with a smaller tile, then image by image, then with Lanczos-3.
3. **Export**: mip chain (sRGB-correct, alpha coverage kept for cut-outs, normals renormalised), encode to the
   original GX2 format, tile, write into the archive, rebuild SARC/Yaz0, reopen and compare everything. Each
   texture must decode back at ≥ 24 dB of the intended image or the original is kept.

## If the game crashes with the packages on

The game halts with a message such as `stageRsrc != (0)`, `modelData != (0)` or a guest null-pointer read
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
| `export [--only TEXT,TEXT]` | rebuild + packages |
| `run [--disc DIR] [--only …]` | all of the above |
| `compare --names TEXT,TEXT [--backends all\|KEY,KEY] [--crop N]` | contact sheet of models in `reports/` |
| `install --mods-root DIR [--no-enable]` | copy packages into a ModManager folder and enable them |
| `stats` | per-class counts and sizes |

`--debug-tint` / `--no-debug-tint` toggles the magenta diagnostics tint.

## Installing and testing with the recomp

The recomp's mod root is `%APPDATA%\wwhd\ModManager` on Windows (`~/.config/wwhd/ModManager` on Linux,
`host::config_dir()`). To test without touching your normal setup:

```bash
texture_studio --headless install --project <project> --mods-root <project>/game_test/ModManager
WWHD_MOD_MANAGER_DIR=<project>/game_test/ModManager wwhd --game <disc> --no-vr
```

The packages appear in the game's Mods → Community list as content mods; turning them off restores the
original textures after a restart.
