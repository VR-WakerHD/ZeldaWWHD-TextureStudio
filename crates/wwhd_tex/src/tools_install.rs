//! Installs the AI upscalers the app supports from their official releases.
//!
//! Each tool is one zip from its project's GitHub release, pinned by size and
//! SHA-256. Model add-ons (4x-AnimeSharp) are ncnn files from their author's own
//! repository, pinned per file and to a commit, saved into `custom-models` and run
//! by Real-ESRGAN's tool. A tool counts as installed when its executable is found the way
//! the upscaler looks for it ([`crate::upscale::Tools::exe`]: up to three
//! folders below the tools folder), so a tool that is missing for any reason
//! (never downloaded, deleted, a fresh clone on Linux) is downloaded again.
//! Nothing downloaded is run here; the upscaler runs the tools later as it
//! always has.

use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::{bail, Error, Result};
use crate::jobs::Progress;

/// One official release archive.
#[derive(Clone, Debug)]
pub struct Release {
    /// Display name ("Real-ESRGAN").
    pub tool: String,
    /// Executable base name the upscaler looks for ("realesrgan-ncnn-vulkan").
    pub exe: String,
    /// Archive file name; its stem is the folder the release is unpacked into.
    pub file: String,
    pub url: String,
    pub size: u64,
    /// Lowercase hex.
    pub sha256: String,
    /// Where its models' licence comes from, for the Settings note.
    pub licence: String,
    /// For a model add-on: its files, saved into `custom-models` (empty for a tool zip).
    pub files: Vec<FilePin>,
}

/// One pinned file of a model add-on.
#[derive(Clone, Debug)]
pub struct FilePin {
    pub url: String,
    /// Saved as `custom-models/<name>`.
    pub name: String,
    pub size: u64,
    /// Lowercase hex.
    pub sha256: String,
}

/// 4x-AnimeSharp by Kim2091: the author's ncnn (fp32) build, pinned to commit 94dab98 of
/// huggingface.co/Kim2091/AnimeSharp. Runs through Real-ESRGAN's tool as the custom model `4x-AnimeSharp`.
fn animesharp() -> Release {
    const AT: &str = "https://huggingface.co/Kim2091/AnimeSharp/resolve/94dab98f2766c5a182c7f86aa7a2eed388f0257c/NCNN/";
    let pin = |file: &str, name: &str, size: u64, sha256: &str| FilePin { url: format!("{AT}{file}"), name: name.into(), size, sha256: sha256.into() };
    let files = vec![
        pin("4x-AnimeSharp-fp32.param", "4x-AnimeSharp.param", 165579, "0332002123306541c803cd675280eee7f9cdd96f0804a47129f86ebbf9ed4174"),
        pin("4x-AnimeSharp-fp32.bin", "4x-AnimeSharp.bin", 66793352, "d71eed4c6000a8ca6016ffc057be82fbc20d8b3c567bc28a1608db1fb58084e6"),
    ];
    Release {
        tool: "4x-AnimeSharp".into(),
        exe: "realesrgan-ncnn-vulkan".into(),
        file: "4x-AnimeSharp".into(),
        url: "https://huggingface.co/Kim2091/AnimeSharp".into(),
        size: files.iter().map(|f| f.size).sum(),
        sha256: String::new(),
        licence: "CC BY-NC-SA 4.0, non-commercial (Kim2091)".into(),
        files,
    }
}

fn r(tool: &str, exe: &str, url: &str, size: u64, sha256: &str, licence: &str) -> Release {
    Release {
        tool: tool.into(),
        exe: exe.into(),
        file: url.rsplit('/').next().unwrap_or(url).into(),
        url: url.into(),
        size,
        sha256: sha256.into(),
        licence: licence.into(),
        files: Vec::new(),
    }
}

/// The releases for this operating system (none where the projects publish no build).
pub fn releases() -> Vec<Release> {
    const ESRGAN: &str = "https://github.com/xinntao/Real-ESRGAN/releases/download/v0.2.5.0/";
    const WAIFU: &str = "https://github.com/nihui/waifu2x-ncnn-vulkan/releases/download/20250915/";
    const CUGAN: &str = "https://github.com/nihui/realcugan-ncnn-vulkan/releases/download/20220728/";
    const REALSR: &str = "https://github.com/nihui/realsr-ncnn-vulkan/releases/download/20220728/";
    let (e, w, c, s) = ("BSD-3-Clause (Real-ESRGAN)", "MIT (waifu2x)", "Real-CUGAN's own terms (bilibili/ailab)", "RealSR's own terms (Tencent)");
    if cfg!(windows) {
        vec![
            r("Real-ESRGAN", "realesrgan-ncnn-vulkan", &format!("{ESRGAN}realesrgan-ncnn-vulkan-20220424-windows.zip"), 45474481, "abc02804e17982a3be33675e4d471e91ea374e65b70167abc09e31acb412802d", e),
            r("waifu2x", "waifu2x-ncnn-vulkan", &format!("{WAIFU}waifu2x-ncnn-vulkan-20250915-windows.zip"), 35497352, "7425be94b94e4c8f37a1e433ac0e0100c43790e2c37418f4b65d8235adfbdc87", w),
            r("Real-CUGAN", "realcugan-ncnn-vulkan", &format!("{CUGAN}realcugan-ncnn-vulkan-20220728-windows.zip"), 45977449, "c6e08d46c11704b1e3a1ada9ddd591cb5005f52f132136c8633ba25def400e01", c),
            r("RealSR", "realsr-ncnn-vulkan", &format!("{REALSR}realsr-ncnn-vulkan-20220728-windows.zip"), 64005701, "db77ca8247642e2b694f1ce0fed3d91f557b75464ff343f5168465fe414223b3", s),
            animesharp(),
        ]
    } else if cfg!(target_os = "linux") {
        vec![
            r("Real-ESRGAN", "realesrgan-ncnn-vulkan", &format!("{ESRGAN}realesrgan-ncnn-vulkan-20220424-ubuntu.zip"), 46931474, "e5aa6eb131234b87c0c51f82b89390f5e3e642b7b70f2b9bbe95b6a285a40c96", e),
            r("waifu2x", "waifu2x-ncnn-vulkan", &format!("{WAIFU}waifu2x-ncnn-vulkan-20250915-linux.zip"), 36658685, "848e0fba55657d34da90b775b8139e9806dc754798b029f95e106ba8850a731f", w),
            r("Real-CUGAN", "realcugan-ncnn-vulkan", &format!("{CUGAN}realcugan-ncnn-vulkan-20220728-ubuntu.zip"), 46704187, "d745174bd04c0232c89d935b74799311008fda06bea4195f61be5f0f3cc087cb", c),
            r("RealSR", "realsr-ncnn-vulkan", &format!("{REALSR}realsr-ncnn-vulkan-20220728-ubuntu.zip"), 64546766, "6007e55c156a6b4600e569c68818836c22daeac63fdf71db068966b5425ac085", s),
            animesharp(),
        ]
    } else {
        Vec::new()
    }
}

/// A release and whether its tool is already in the tools folder.
#[derive(Clone, Debug)]
pub struct Status {
    pub release: Release,
    pub installed: bool,
}

fn exe_name(base: &str) -> String {
    if cfg!(windows) { format!("{base}.exe") } else { base.to_string() }
}

/// Which of the releases' tools are in `dir`.
pub fn status_of(dir: &Path, releases: &[Release]) -> Vec<Status> {
    releases
        .iter()
        .map(|r| {
            let installed = if r.files.is_empty() {
                crate::upscale::find_file(dir, &exe_name(&r.exe), 3).is_some()
            } else {
                r.files.iter().all(|f| dir.join("custom-models").join(&f.name).is_file())
            };
            Status { release: r.clone(), installed }
        })
        .collect()
}

/// [`status_of`] for this system's releases.
pub fn status(dir: &Path) -> Vec<Status> {
    status_of(dir, &releases())
}

/// The archive's size and SHA-256 must match the pins.
pub fn verify(bytes: &[u8], r: &Release) -> Result<()> {
    verify_pin(bytes, &r.file, r.size, &r.sha256)
}

fn verify_pin(bytes: &[u8], what: &str, size: u64, sha256: &str) -> Result<()> {
    if bytes.len() as u64 != size {
        bail!("{what}: downloaded {} bytes, the official release has {size}", bytes.len());
    }
    let got: String = Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect();
    if got != sha256 {
        bail!("{what}: SHA-256 {got} does not match the official release's {sha256}");
    }
    Ok(())
}

/// Saves a model add-on's checked files into `dir/custom-models`: every file is checked first and written under a
/// temporary name, then all are renamed into place, so a failure leaves no half model behind.
fn save_model(dir: &Path, r: &Release, files: &[(FilePin, Vec<u8>)]) -> Result<()> {
    for (pin, bytes) in files {
        verify_pin(bytes, &pin.name, pin.size, &pin.sha256)?;
    }
    let models = dir.join("custom-models");
    std::fs::create_dir_all(&models)?;
    let mut written = Vec::new();
    for (pin, bytes) in files {
        let tmp = models.join(format!(".{}.partial", pin.name));
        if let Err(e) = std::fs::write(&tmp, bytes) {
            for t in &written {
                let _ = std::fs::remove_file(t);
            }
            return Err(e.into());
        }
        written.push(tmp);
    }
    for (pin, _) in files {
        std::fs::rename(models.join(format!(".{}.partial", pin.name)), models.join(&pin.name))?;
    }
    let _ = r;
    Ok(())
}

/// Unpacks `zip` into `dir/<folder>`: first into a temporary folder beside it, renamed into place only when every
/// entry is out, so a failure leaves nothing half-installed. Entries that would land outside the folder (absolute
/// paths, `..`) refuse the whole archive. An existing `<folder>` (the tool is missing from it) is kept, renamed to
/// `<folder>.old`, `<folder>.old2`, …
pub fn unpack(zip: &[u8], dir: &Path, folder: &str) -> Result<PathBuf> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip)).map_err(|e| Error::Format(format!("{folder}: {e}")))?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{folder}.partial"));
    if tmp.exists() {
        std::fs::remove_dir_all(&tmp)?;
    }
    let result = (|| -> Result<()> {
        for i in 0..archive.len() {
            let mut entry = archive.by_index(i).map_err(|e| Error::Format(format!("{folder}: {e}")))?;
            let Some(rel) = entry.enclosed_name() else {
                bail!("{folder}: entry {:?} would be unpacked outside the tools folder", entry.name());
            };
            let out = tmp.join(rel);
            if entry.is_dir() {
                std::fs::create_dir_all(&out)?;
                continue;
            }
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut f = std::fs::File::create(&out)?;
            std::io::copy(&mut entry, &mut f)?;
            #[cfg(unix)]
            if let Some(mode) = entry.unix_mode() {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&out, std::fs::Permissions::from_mode(mode & 0o777))?;
            }
        }
        Ok(())
    })();
    if let Err(e) = result {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(e);
    }
    let dest = dir.join(folder);
    if dest.exists() {
        let mut n = 1;
        let old = loop {
            let o = dir.join(if n == 1 { format!("{folder}.old") } else { format!("{folder}.old{n}") });
            if !o.exists() {
                break o;
            }
            n += 1;
        };
        std::fs::rename(&dest, &old)?;
    }
    std::fs::rename(&tmp, &dest)?;
    Ok(dest)
}

/// Downloads `url` (expected `size` bytes) over HTTPS, calling `on_bytes` with the bytes read so far; stops when
/// `cancelled` turns true.
pub fn download(url: &str, size: u64, on_bytes: &dyn Fn(u64), cancelled: &dyn Fn() -> bool) -> Result<Vec<u8>> {
    let resp = ureq::get(url).call().map_err(|e| Error::Io(std::io::Error::other(format!("{url}: {e}"))))?;
    let mut reader = resp.into_reader().take(size + 1);
    let mut bytes = Vec::with_capacity(size as usize);
    let mut buf = vec![0u8; 1 << 16];
    loop {
        if cancelled() {
            return Err(Error::Cancelled);
        }
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&buf[..n]);
        on_bytes(bytes.len() as u64);
    }
    Ok(bytes)
}

/// Downloads `(url, size)`, reporting bytes read so far (tests pass their own).
pub type Fetch<'a> = &'a dyn Fn(&str, u64, &dyn Fn(u64)) -> Result<Vec<u8>>;

/// Downloads, checks and unpacks every release in `releases` whose tool is not in `dir`, with `fetch` doing the
/// download. Model add-ons are saved into `custom-models`. Returns the tools installed. Stops at the first failure,
/// keeping the tools already installed.
pub fn install_missing_with(dir: &Path, releases: &[Release], progress: &Progress, fetch: Fetch) -> Result<Vec<String>> {
    let todo: Vec<Release> = status_of(dir, releases).into_iter().filter(|s| !s.installed).map(|s| s.release).collect();
    let mib = |b: u64| (b / (1 << 20)) as usize;
    progress.begin("Installing upscaling models", todo.iter().map(|r| mib(r.size).max(1)).sum());
    let mut done = Vec::new();
    for r in &todo {
        if progress.cancelled() {
            return Err(Error::Cancelled);
        }
        progress.item(&format!("{} ({} MiB)", r.tool, mib(r.size)));
        let counted = std::cell::Cell::new(0usize);
        let before = std::cell::Cell::new(0u64);
        let on = |n: u64| {
            let now = mib(before.get() + n).min(mib(r.size).max(1));
            progress.steps(now.saturating_sub(counted.get()));
            counted.set(counted.get().max(now));
        };
        if !r.files.is_empty() {
            let mut got = Vec::new();
            for pin in &r.files {
                let bytes = fetch(&pin.url, pin.size, &on)?;
                before.set(before.get() + pin.size);
                got.push((pin.clone(), bytes));
            }
            progress.steps(mib(r.size).max(1).saturating_sub(counted.get()));
            save_model(dir, r, &got)?;
            progress.log(format!("installed {} into {}", r.tool, dir.join("custom-models").display()));
            done.push(r.tool.clone());
            continue;
        }
        let bytes = fetch(&r.url, r.size, &on)?;
        progress.steps(mib(r.size).max(1).saturating_sub(counted.get()));
        verify(&bytes, r)?;
        let stem = r.file.trim_end_matches(".zip");
        let dest = unpack(&bytes, dir, stem)?;
        if crate::upscale::find_file(&dest, &exe_name(&r.exe), 3).is_none() {
            bail!("{}: the official archive did not contain {}", r.file, exe_name(&r.exe));
        }
        progress.log(format!("installed {} into {}", r.tool, dest.display()));
        done.push(r.tool.clone());
    }
    Ok(done)
}

/// [`install_missing_with`] for this system's releases, downloading over HTTPS.
pub fn install_missing(dir: &Path, progress: &Progress) -> Result<Vec<String>> {
    if releases().is_empty() {
        crate::error::unsupported!("no official upscaler builds for this operating system");
    }
    install_missing_with(dir, &releases(), progress, &|url, size, on| download(url, size, on, &|| progress.cancelled()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("wwhd-tools-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn zip_of(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, data) in files {
            w.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
            w.write_all(data).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    fn release_for(tool: &str, exe: &str, bytes: &[u8]) -> Release {
        Release {
            tool: tool.into(),
            exe: exe.into(),
            file: format!("{exe}-test.zip"),
            url: tool.into(),
            size: bytes.len() as u64,
            sha256: Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect(),
            licence: String::new(),
            files: Vec::new(),
        }
    }

    #[test]
    fn pins_cover_every_supported_tool() {
        let rs = releases();
        if cfg!(any(windows, target_os = "linux")) {
            let tools: Vec<&str> = rs.iter().map(|r| r.tool.as_str()).collect();
            assert_eq!(tools, ["Real-ESRGAN", "waifu2x", "Real-CUGAN", "RealSR", "4x-AnimeSharp"]);
            for r in rs.iter().filter(|r| r.files.is_empty()) {
                assert!(r.url.starts_with("https://github.com/") && r.url.ends_with(&r.file), "{r:?}");
                assert_eq!(r.sha256.len(), 64, "{r:?}");
                assert!(r.size > 1 << 20, "{r:?}");
            }
        }
    }

    #[test]
    fn a_zip_with_an_escaping_entry_installs_nothing() {
        let dir = temp("escape");
        let z = zip_of(&[("ok/a.txt", b"a"), ("../evil.txt", b"x")]);
        assert!(unpack(&z, &dir, "tool").is_err());
        assert!(!dir.join("tool").exists());
        assert!(!dir.join(".tool.partial").exists());
        assert!(!dir.parent().unwrap().join("evil.txt").exists());
    }

    #[test]
    fn a_checksum_mismatch_installs_nothing() {
        let dir = temp("sum");
        let exe = exe_name("faketool-ncnn-vulkan");
        let z = zip_of(&[(&format!("faketool/{exe}"), b"exe")]);
        let mut r = release_for("Fake", "faketool-ncnn-vulkan", &z);
        r.sha256 = "0".repeat(64);
        let err = install_missing_with(&dir, &[r], &Progress::default(), &|_, _, _| Ok(z.clone())).unwrap_err();
        assert!(err.to_string().contains("SHA-256"), "{err}");
        assert!(std::fs::read_dir(&dir).unwrap().next().is_none(), "nothing may be left behind");
    }

    #[test]
    fn missing_tools_are_installed_and_installed_ones_skipped() {
        let dir = temp("missing");
        let a = zip_of(&[(&format!("a-1/a-1/{}", exe_name("atool-ncnn-vulkan")), b"a"), ("a-1/a-1/models/m.bin", b"m")]);
        let b = zip_of(&[(&format!("b-1/{}", exe_name("btool-ncnn-vulkan")), b"b")]);
        let rels = [release_for("A", "atool-ncnn-vulkan", &a), release_for("B", "btool-ncnn-vulkan", &b)];
        // B is already installed somewhere below the tools folder (as a bundled release would be)
        std::fs::create_dir_all(dir.join("bundled/b")).unwrap();
        std::fs::write(dir.join("bundled/b").join(exe_name("btool-ncnn-vulkan")), b"b").unwrap();
        let asked = std::cell::RefCell::new(Vec::new());
        let got = install_missing_with(&dir, &rels, &Progress::default(), &|url, size, on| {
            asked.borrow_mut().push(url.to_string());
            on(size);
            Ok(if url == "A" { a.clone() } else { b.clone() })
        })
        .unwrap();
        assert_eq!(got, ["A"]);
        assert_eq!(*asked.borrow(), ["A"]);
        let folder = dir.join(rels[0].file.trim_end_matches(".zip"));
        assert!(folder.join("a-1/a-1").join(exe_name("atool-ncnn-vulkan")).is_file());
        assert!(folder.join("a-1/a-1/models/m.bin").is_file());
        assert!(status_of(&dir, &rels).iter().all(|s| s.installed));
        // nothing left to do: a second run asks for nothing
        asked.borrow_mut().clear();
        assert!(install_missing_with(&dir, &rels, &Progress::default(), &|_, _, _| unreachable!()).unwrap().is_empty());
    }

    fn model_release(files: &[(&str, &[u8])]) -> Release {
        let pins: Vec<FilePin> = files
            .iter()
            .map(|(name, data)| FilePin {
                url: format!("https://example.invalid/{name}"),
                name: (*name).into(),
                size: data.len() as u64,
                sha256: Sha256::digest(data).iter().map(|b| format!("{b:02x}")).collect(),
            })
            .collect();
        Release { tool: "Model".into(), exe: "realesrgan-ncnn-vulkan".into(), file: "Model".into(), url: String::new(), size: pins.iter().map(|p| p.size).sum(), sha256: String::new(), licence: String::new(), files: pins }
    }

    #[test]
    fn a_model_add_on_is_saved_into_custom_models_after_every_file_checks_out() {
        let dir = temp("model");
        let (param, bin) = (b"7767517\n1 1\n".to_vec(), vec![7u8; 4096]);
        let r = model_release(&[("m.param", &param), ("m.bin", &bin)]);
        assert!(!status_of(&dir, std::slice::from_ref(&r))[0].installed);
        let asked = std::cell::RefCell::new(Vec::new());
        let got = install_missing_with(&dir, std::slice::from_ref(&r), &Progress::default(), &|url, _, _| {
            asked.borrow_mut().push(url.to_string());
            Ok(if url.ends_with(".param") { param.clone() } else { bin.clone() })
        })
        .unwrap();
        assert_eq!(got, ["Model"]);
        assert_eq!(asked.borrow().len(), 2);
        assert_eq!(std::fs::read(dir.join("custom-models/m.param")).unwrap(), param);
        assert_eq!(std::fs::read(dir.join("custom-models/m.bin")).unwrap(), bin);
        assert!(status_of(&dir, &[r])[0].installed);
    }

    #[test]
    fn a_model_with_one_bad_file_saves_nothing() {
        let dir = temp("model-bad");
        let r = model_release(&[("m.param", b"good"), ("m.bin", b"good-bin")]);
        let err = install_missing_with(&dir, &[r], &Progress::default(), &|url, _, _| Ok(if url.ends_with(".param") { b"good".to_vec() } else { b"tampered".to_vec() })).unwrap_err();
        assert!(err.to_string().contains("m.bin"), "{err}");
        let left: Vec<_> = std::fs::read_dir(dir.join("custom-models")).map(|d| d.flatten().collect()).unwrap_or_default();
        assert!(left.is_empty(), "no half model: {left:?}");
    }

    #[test]
    fn animesharp_is_pinned_to_the_authors_commit_and_saved_under_the_known_name() {
        let r = animesharp();
        assert_eq!(r.files.len(), 2);
        for f in &r.files {
            assert!(f.url.starts_with("https://huggingface.co/Kim2091/AnimeSharp/resolve/94dab98f2766c5a182c7f86aa7a2eed388f0257c/NCNN/"), "{}", f.url);
            assert_eq!(f.sha256.len(), 64);
        }
        let names: Vec<&str> = r.files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["4x-AnimeSharp.param", "4x-AnimeSharp.bin"]);
        assert!(crate::upscale::KNOWN_CUSTOM.iter().any(|k| format!("{}.param", k.stem) == names[0]), "the upscaler finds it by this name");
    }

    #[test]
    fn an_existing_folder_without_the_tool_is_kept_aside() {
        let dir = temp("aside");
        let exe = exe_name("ctool-ncnn-vulkan");
        let z = zip_of(&[(exe.as_str(), b"c")]);
        let r = release_for("C", "ctool-ncnn-vulkan", &z);
        let stem = r.file.trim_end_matches(".zip").to_string();
        std::fs::create_dir_all(dir.join(&stem)).unwrap();
        std::fs::write(dir.join(&stem).join("notes.txt"), b"mine").unwrap();
        install_missing_with(&dir, &[r], &Progress::default(), &|_, _, _| Ok(z.clone())).unwrap();
        assert!(dir.join(&stem).join(&exe).is_file());
        assert_eq!(std::fs::read(dir.join(format!("{stem}.old")).join("notes.txt")).unwrap(), b"mine");
    }
}
