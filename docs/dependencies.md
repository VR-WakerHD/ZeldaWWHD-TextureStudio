# Dependencies and licences

Checked 2026-11-02. Bundled in the repository: `third_party/wiiu_swizzle` (MIT, with its licence file) and two
upscaler releases whose code and model licences allow redistribution, Real-ESRGAN ncnn Vulkan and waifu2x ncnn
Vulkan (Windows builds, with their licence files; `tools/upscalers/README.md`). Real-CUGAN and RealSR are downloaded
by the user from the official release pages.

## Rust crates (built from source via crates.io)

| Crate | Version | Licence | Use |
|---|---|---|---|
| eframe / egui | 0.37 | MIT OR Apache-2.0 | GUI |
| rfd | 0.17 | MIT | native folder picker |
| image | 0.25 | MIT OR Apache-2.0 | PNG read/write |
| image_dds | 0.7.2 | MIT | BC1–BC5 encode (Intel ISPC via intel_tex_2) and decode (bcdec_rs) |
| intel_tex_2 | 0.5 | MIT/Apache-2.0 | BC encoder (precompiled ISPC kernels shipped in the crate) |
| bcdec_rs | 0.2 | MIT | BC decoder |
| xxhash-rust | 0.8 | BSL-1.0 | texture keys: XXH3-64 of level-0 guest bytes (same hash the recomp uses at run time) |
| blake3 | 1.8 | CC0-1.0 OR Apache-2.0 | content hashes |
| ureq | 2 | MIT OR Apache-2.0 | HTTPS download of the official upscaler releases (rustls, webpki roots) |
| zip | 2 (deflate only) | MIT | unpacking those releases |
| sha2 | 0.10 | MIT OR Apache-2.0 | SHA-256 check of each release against its pin |
| rayon | 1.12 | MIT OR Apache-2.0 | parallel scan/extract/export |
| serde / serde_json | 1.0 | MIT OR Apache-2.0 | manifests, settings |
| bilge (via wiiu_swizzle) | 0.2 | MIT OR Apache-2.0 | bitfields |

## Vendored

| Component | Source | Licence | Changes |
|---|---|---|---|
| wiiu_swizzle 0.3.0 | https://github.com/ScanMountGoat/wiiu_swizzle | MIT | `addrlib` made public; three R6xx-only paddings disabled for Latte; out-of-range tiling returns an error instead of panicking; `swizzle_mipmap_sized` added. Each change is marked "ZeldaWWHD Texture Studio patch" |

wiiu_swizzle itself credits decaf-emu/addrlib (Mesa-derived) and Cemu constants.

## Upscaler tools (Real-ESRGAN and waifu2x bundled; Real-CUGAN and RealSR user download)

| Tool | Release | Licence | Models included | Backend | Notes |
|---|---|---|---|---|---|
| realesrgan-ncnn-vulkan | Real-ESRGAN v0.2.5.0 (`20220424`) | BSD-3-Clause (Real-ESRGAN), MIT (ncnn port) | realesrgan-x4plus, realesrgan-x4plus-anime, realesr-animevideov3 x2/x3/x4 | ncnn + Vulkan | GPU only (no CPU mode); x4plus models are 4× only |
| waifu2x-ncnn-vulkan | 20250915 | MIT | cunet, upconv_7_anime_style_art_rgb, upconv_7_photo | ncnn + Vulkan, CPU | `cunet` with noise 0 produced garbage on the RTX 5080 in testing; caught by the consistency check |
| realcugan-ncnn-vulkan | 20220728 | MIT | models-se (2–4×), models-pro (2–3×), models-nose (2×) | ncnn + Vulkan, CPU | |
| realsr-ncnn-vulkan | 20220728 | MIT | DF2K, DF2K_JPEG (4×) | ncnn + Vulkan, CPU | photographic; adds grain to WWHD art |

Settings → Upscaling models → **Install Upscaling Models** (and `install-tools` on the command line) downloads
the missing ones from these exact URLs and checks size and SHA-256 before unpacking (`crates/wwhd_tex/src/tools_install.rs`).
The Linux pins were taken from the official `-ubuntu`/`-linux` archives on 2026-11-02 (waifu2x's is GitHub's own
digest).

SHA-256 of the downloaded archives (Windows):

```
abc02804e17982a3be33675e4d471e91ea374e65b70167abc09e31acb412802d realesrgan-ncnn-vulkan-20220424-windows.zip
7425be94b94e4c8f37a1e433ac0e0100c43790e2c37418f4b65d8235adfbdc87 waifu2x-ncnn-vulkan-20250915-windows.zip
c6e08d46c11704b1e3a1ada9ddd591cb5005f52f132136c8633ba25def400e01 realcugan-ncnn-vulkan-20220728-windows.zip
db77ca8247642e2b694f1ce0fed3d91f557b75464ff343f5168465fe414223b3 realsr-ncnn-vulkan-20220728-windows.zip
```

Model weight licences follow their projects: Real-ESRGAN BSD-3-Clause (bundled, `LICENSE-Real-ESRGAN.txt` copied
from xinntao/Real-ESRGAN, as the release archive has none) and waifu2x MIT (bundled). Real-CUGAN's models come from
bilibili/ailab, which has no licence file, and RealSR's follow Tencent's RealSR terms; neither is cleared for
redistribution, so those two are not bundled. The bundled releases ship without their demo images/video and
without `vcomp140d.dll` (a Microsoft debug DLL that is not redistributable); `vcomp140.dll` is Microsoft's
redistributable OpenMP runtime.

## Backend choice

ncnn/Vulkan was chosen over ONNX Runtime/TensorRT because the upstream projects publish ready ncnn models and
Vulkan binaries for Windows and Linux. ONNX would need PyTorch conversion of the `.pth` weights plus a
CUDA/cuDNN installation (absent on the development PC) and a second backend for non-NVIDIA GPUs.
On the RTX 5080 Real-ESRGAN x4plus-anime runs at about 0.4 s per 256–512 px texture including process start-up
amortised over 32-texture batches.

## Rejected / not used

- Switch Toolbox, BFRES-Tool: C#/Windows-only GUIs, not libraries; this app only needs FTEX/BFLIM, which is
  implemented and verified directly.
- Cemu's LatteAddrLib (used by the recomp, MPL-2.0): equivalent to the vendored addrlib; not needed.
