//! Installs the AI upscalers the app supports from their official releases.
//!
//! Each tool is one zip from its project's GitHub release, pinned by size and
//! SHA-256. A tool counts as installed when its executable is found the way
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
        ]
    } else if cfg!(target_os = "linux") {
        vec![
            r("Real-ESRGAN", "realesrgan-ncnn-vulkan", &format!("{ESRGAN}realesrgan-ncnn-vulkan-20220424-ubuntu.zip"), 46931474, "e5aa6eb131234b87c0c51f82b89390f5e3e642b7b70f2b9bbe95b6a285a40c96", e),
            r("waifu2x", "waifu2x-ncnn-vulkan", &format!("{WAIFU}waifu2x-ncnn-vulkan-20250915-linux.zip"), 36658685, "848e0fba55657d34da90b775b8139e9806dc754798b029f95e106ba8850a731f", w),
            r("Real-CUGAN", "realcugan-ncnn-vulkan", &format!("{CUGAN}realcugan-ncnn-vulkan-20220728-ubuntu.zip"), 46704187, "d745174bd04c0232c89d935b74799311008fda06bea4195f61be5f0f3cc087cb", c),
            r("RealSR", "realsr-ncnn-vulkan", &format!("{REALSR}realsr-ncnn-vulkan-20220728-ubuntu.zip"), 64546766, "6007e55c156a6b4600e569c68818836c22daeac63fdf71db068966b5425ac085", s),
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
        .map(|r| Status { release: r.clone(), installed: crate::upscale::find_file(dir, &exe_name(&r.exe), 3).is_some() })
        .collect()
}

/// [`status_of`] for this system's releases.
pub fn status(dir: &Path) -> Vec<Status> {
    status_of(dir, &releases())
}

/// The archive's size and SHA-256 must match the pins.
pub fn verify(bytes: &[u8], r: &Release) -> Result<()> {
    if bytes.len() as u64 != r.size {
        bail!("{}: downloaded {} bytes, the official release has {}", r.file, bytes.len(), r.size);
    }
    let got: String = Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect();
    if got != r.sha256 {
        bail!("{}: SHA-256 {got} does not match the official release's {}", r.file, r.sha256);
    }
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

/// Downloads `r` over HTTPS, calling `on_bytes` with the bytes read so far; stops when `cancelled` turns true.
pub fn download(r: &Release, on_bytes: &dyn Fn(u64), cancelled: &dyn Fn() -> bool) -> Result<Vec<u8>> {
    let resp = ureq::get(&r.url).call().map_err(|e| Error::Io(std::io::Error::other(format!("{}: {e}", r.url))))?;
    let mut reader = resp.into_reader().take(r.size + 1);
    let mut bytes = Vec::with_capacity(r.size as usize);
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

/// Downloads, checks and unpacks every release in `releases` whose tool is not in `dir`, with `fetch` doing the
/// download (tests pass their own). Returns the tools installed. Stops at the first failure, keeping the tools
/// already installed.
pub fn install_missing_with(
    dir: &Path,
    releases: &[Release],
    progress: &Progress,
    fetch: &dyn Fn(&Release, &dyn Fn(u64)) -> Result<Vec<u8>>,
) -> Result<Vec<String>> {
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
        let bytes = fetch(r, &|n| {
            let now = mib(n).min(mib(r.size).max(1));
            progress.steps(now.saturating_sub(counted.get()));
            counted.set(counted.get().max(now));
        })?;
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
    install_missing_with(dir, &releases(), progress, &|r, on| download(r, on, &|| progress.cancelled()))
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
            url: String::new(),
            size: bytes.len() as u64,
            sha256: Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect(),
            licence: String::new(),
        }
    }

    #[test]
    fn pins_cover_every_supported_tool() {
        let rs = releases();
        if cfg!(any(windows, target_os = "linux")) {
            let tools: Vec<&str> = rs.iter().map(|r| r.tool.as_str()).collect();
            assert_eq!(tools, ["Real-ESRGAN", "waifu2x", "Real-CUGAN", "RealSR"]);
            for r in &rs {
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
        let err = install_missing_with(&dir, &[r], &Progress::default(), &|_, _| Ok(z.clone())).unwrap_err();
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
        let got = install_missing_with(&dir, &rels, &Progress::default(), &|r, on| {
            asked.borrow_mut().push(r.tool.clone());
            on(r.size);
            Ok(if r.tool == "A" { a.clone() } else { b.clone() })
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
        assert!(install_missing_with(&dir, &rels, &Progress::default(), &|_, _| unreachable!()).unwrap().is_empty());
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
        install_missing_with(&dir, &[r], &Progress::default(), &|_, _| Ok(z.clone())).unwrap();
        assert!(dir.join(&stem).join(&exe).is_file());
        assert_eq!(std::fs::read(dir.join(format!("{stem}.old")).join("notes.txt")).unwrap(), b"mine");
    }
}
