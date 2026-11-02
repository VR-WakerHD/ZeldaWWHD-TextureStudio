# Runtime texture replacement — design note for the recomp (not implemented)

> Superseded by the full design in `docs/superpowers/specs/2026-11-02-runtime-texture-adapter-design.md`
> (key = XXH3-64 of level 0 rather than blake3; replacement bound per draw, not in place).

The current integration rebuilds game archives. That is safe and needs no game change, but it is limited:

- textures must stay in their GX2 format (no BC7 on PCVR, no ASTC on Quest 3 / Steam Frame);
- upscaled data lives in the game's guest memory and archive heaps;
- the packages are large because whole archives are replaced.

A runtime adapter in ZeldaWWHD-VR would remove these limits.

## Where it would hook

The recomp's renderers create host images from GX2 surfaces in `runtime/src/gfx/vulkan/surfaces.cpp`
(and the Metal equivalent), after detiling with LatteAddrLib. At that point the guest texture's base address,
format, dimensions, tile mode and data are known. The adapter would:

1. hash the guest texture data (first level, tiled bytes) — the same blake3 `data_hash` this app records in
   `manifests/replacement_manifest.json` (`original_hash`), plus format and dimensions as a guard;
2. look the hash up in a table loaded from an installed replacement pack;
3. if found, create the host image from the pack's file (DDS BC7/BC5 on desktop, KTX2 ASTC on Android)
   instead of the guest data, keeping the guest texture's sampler and view semantics (component selector);
4. fall back to the guest data on any mismatch.

Hashing must be cached per guest allocation so it costs nothing per frame.

## What this app already provides

- `original_hash`, format, dimensions and texture name per replacement in `replacement_manifest.json`;
- lossless upscaled PNGs per texture in `cache/upscaled/...`;
- class information to choose BC7 vs BC5 vs BC4 and ASTC block sizes.

Adding a DDS/KTX2 writer for those outputs is small once the game side exists.

## Platform notes (unverified until the adapter exists)

- PCVR (Vulkan on desktop GPUs): BC7 for colour, BC5 for normals, BC4 for masks.
- Quest 3 (Adreno 740, Vulkan 1.3): ASTC LDR is supported in hardware; BC formats are not. ASTC 4×4 or 6×6
  for colour.
- Steam Frame: hardware and driver format support must be checked on the device before choosing formats.
