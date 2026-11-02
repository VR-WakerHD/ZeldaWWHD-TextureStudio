# Upscaler tools

Texture Studio runs these official ncnn/Vulkan releases as child processes. It finds each `*-ncnn-vulkan.exe` up to
three folders below this one, so each release is kept as it was unpacked.

## Included (Windows builds, unchanged from the official releases)

| Folder | Release | Code licence | Model licence |
|---|---|---|---|
| `realesrgan-ncnn-vulkan-20220424-windows` | [Real-ESRGAN v0.2.5.0](https://github.com/xinntao/Real-ESRGAN/releases/tag/v0.2.5.0) | MIT (`LICENSE-Real-ESRGAN-ncnn-vulkan.txt`) | BSD-3-Clause (`LICENSE-Real-ESRGAN.txt`) |
| `waifu2x-ncnn-vulkan-20250915-windows` | [waifu2x-ncnn-vulkan 20250915](https://github.com/nihui/waifu2x-ncnn-vulkan/releases/tag/20250915) | MIT (`LICENSE`) | MIT (nagadomi/waifu2x) |

The licence files above came with the releases, or are copied verbatim from the official repositories where the
release archive had none (Real-ESRGAN). `vcomp140.dll` is Microsoft's redistributable OpenMP runtime that the releases
ship with.

Left out: the releases' demo images and video, and `vcomp140d.dll` (a Microsoft debug DLL, not redistributable).

## Not included (download them yourself if you want them)

| Tool | Official release | Why not included |
|---|---|---|
| Real-CUGAN ncnn Vulkan | https://github.com/nihui/realcugan-ncnn-vulkan/releases/tag/20220728 | code is MIT, but the models' source (bilibili/ailab) has no licence file |
| RealSR ncnn Vulkan | https://github.com/nihui/realsr-ncnn-vulkan/releases/tag/20220728 | code is MIT, but the models follow Tencent's RealSR terms, not cleared |

Unpack either into its own folder here and Texture Studio picks it up. Custom ESRGAN-family ncnn models
(`name.param` + `name.bin`, 4×) go in `custom-models/`. Both stay local (git-ignored).

SHA-256 of the official archives is listed in `docs/dependencies.md`.
