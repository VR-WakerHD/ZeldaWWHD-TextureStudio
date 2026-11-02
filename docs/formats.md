# Supported resource formats

Verified on the USA Wind Waker HD dump (`game_disc`, 1.7 GB of content) on 2026-11-02 with the `gate` example
(`cargo run --release -p wwhd_tex --example gate -- <disc>/content`).

## Containers

| Format | Where | Support |
|---|---|---|
| Yaz0 (SZS) | ~800 `.szs` under `content/Common` and inside packs | Decompress + recompress (hash-chain LZ). Header alignment bytes preserved |
| SARC (big-endian) | every `.szs`, `Pack/*.pack`, `Jpeg/Jpeg.sarc` | Parse, nested walk (packs contain `.szs` that contain BFRES/BFLIM), rebuild keeping header/SFAT/SFNT bytes, order, hashes and per-file alignment. Rebuilding an unchanged archive is byte-identical (tested on real archives) |
| BFRES v3.4.0.1 (Wii U FRES) | models, stages, particles, program textures | Texture index group + FTEX parsed; everything else treated as opaque bytes. Replacement appends data and repoints one FTEX |
| BFLIM (Wii U) | `timg/*.bflim` in layouts | Footer (FLIM + imag) parsed; rebuild writes new data + updated footer |
| SHARCFB, BFLYT, BFLAN, JPEG, other | shaders, layouts, animations | Inventoried by type; not textures. JPEG files in `Jpeg.sarc` are reported but not processed |

Path safety: archive entry names with `..`, absolute paths or drive letters are rejected. Nesting depth is
limited to 6; Yaz0 output is capped at 512 MiB.

## Texture formats found

| Container | GX2 format | Tile mode | Count | Rewritable |
|---|---|---|---|---|
| BFRES | BC1 sRGB | 4 (2D thin) / 2 (1D) | 6242 | all |
| BFRES | BC1 UNORM | 4 | 6 | all |
| BFRES | BC3 sRGB | 4 / 2 | 1855 | all but 2 |
| BFRES | BC3 UNORM | 4 | 4 | all |
| BFRES | BC3 UNORM cube map (6 slices) | 4 | 8 | none (cube maps not supported yet) |
| BFRES | BC4 UNORM | 4 / 2 | 1335 | all |
| BFRES | BC5 UNORM / SNORM | 4 / 2 | 666 | all but 2 |
| BFRES | R8G8B8A8 UNORM | 4 | 25 | all |
| BFRES | R8 UNORM | 4 | 2 | all |
| BFLIM | BC1 sRGB | 4 | 194 | all |
| BFLIM | BC3 sRGB | 4 | 266 | all |
| BFLIM | BC4 UNORM | 4 / 2 | 262 | all |
| BFLIM | BC5 UNORM | 4 / 2 | 3196 | all |
| BFLIM | R8G8B8A8 sRGB | 4 | 484 | all |
| BFLIM | R8G8 / R8 UNORM | 4 | 27 | all |

Total 14 572 texture records, 5 499 unique by content.

## The rewrite gate

A texture is only rewritten if, for the original surface:

1. recomputing its layout with addrlib reproduces the stored image size, pitch, alignment, mip size and all
   13 mip offsets;
2. its stored texture registers are consistent (all WWHD files store zero registers; the game computes them
   at load time, so the zero registers are kept);
3. detiling and re-tiling its own data reproduces the stored bytes exactly.

Result: 14 561 of 14 572 pass. The 11 that do not (8 cube maps; `smallcloud_i8`, `AK_swordRoom00`,
`Triforce01` with mip offsets 256 bytes larger than addrlib computes) are kept original and listed in
Diagnostics.

To get there, the vendored `wiiu_swizzle` addrlib port was corrected: three paddings that upstream addrlib
applies only to the R6xx chip family (`macroTileBytes *= 2`, dual base alignment, dual pitch alignment) were
applied unconditionally. Latte follows the non-R6xx paths; with them off, every stored layout is reproduced.

## Channel semantics

Masters keep the stored channels (needed for exact re-encoding). Previews apply the FTEX component selector
(BFRES) or luminance/alpha semantics (BFLIM BC4/BC5/R8/RG8).

## Not supported

- Cube maps and array textures (detected, kept original).
- ETC/other exotic BFLIM formats (none on the WWHD disc; would be reported as unsupported).
- BFRES versions other than 3.x (Switch BFRES v4+/v5+ is a different format).
