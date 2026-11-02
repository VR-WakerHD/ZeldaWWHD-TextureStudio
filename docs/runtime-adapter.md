# Runtime texture replacement — implemented (see the spec)

Status: implemented in both repos on the `texture-packs` branches, covered by unit and smoke tests. **Not yet
verified in the game or in the headset.**

The design, formats and rules are in `docs/superpowers/specs/2026-11-02-runtime-texture-adapter-design.md`; that
document is the source of truth. Usage is in `docs/user-guide.md`; the recomp side is in its `docs/mod-manager.md`
(kind `textures`) and `docs/VR_SETTINGS.md` (`vr.texturePacks`, `vr.texturePackBudgetMB`).

## What changed against the old outline

| Old outline | Implemented |
|---|---|
| Key = blake3 `data_hash` of the tiled level-0 bytes | Key = GX2 format, width, height and XXH3-64 of the level-0 guest bytes (`runtime_pack::key_hash`) |
| Create the host image from the pack file instead of the guest data, in `surfaces.cpp` | The guest image is kept; the replacement is a separate image bound per draw (`replacement_for`), so copies, render targets and invalidation never see it |
| Hash cached per guest allocation | Hashed once per upload of a texture whose bytes may have changed; level 0's hash is kept on the surface |
| One output per platform: BC7 on PCVR, ASTC on Quest | BC only: BC1/2/3 → BC7, BC4 → BC4, BC5 → BC5, RGBA8 → BC7 (RGBA8 for UI and lettering); no ASTC |
| Scale by class only | ×2, ×4, ×8, ×16, ×32 globally, per class or per texture; a scale that cannot be reached steps down one at a time, never skipping |
| Not said | New mod-manager package kind `textures` (`textures_dir`), switched on after a restart; setting `vr.texturePacks`; VRAM budget `vr.texturePackBudgetMB`; images created at most 16 MiB / about 1 ms per frame; any mismatch or bad entry keeps the original |

## What is still open

- A check in the game and in the headset (picture, frame rate, VRAM, the loading pace of the first frames).
- ASTC output for Quest 3 and Steam Frame.
- Textures the runtime pack cannot replace (cube maps, textures that are fetched or sampled by LOD in a shader, with
  hand-made mips) stay original by design.
