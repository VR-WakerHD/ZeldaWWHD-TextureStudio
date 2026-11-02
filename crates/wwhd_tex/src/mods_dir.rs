//! Finds the recomp's mod manager folder the way the recomp does
//! (`runtime/src/mods/packages.cpp` `initialize` and `runtime/src/platform/host.h` `config_dir`), so `install`
//! needs no folder argument. `detect` is pure: every environment input comes through `Env`.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Which rule produced the folder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModsRule {
    /// A folder given by the user (command line or Settings).
    Explicit,
    /// `WWHD_MOD_MANAGER_DIR`.
    ManagerEnv,
    /// `WWHD_USER_DIR`/ModManager.
    UserDirEnv,
    /// `portable.txt` next to the game's executable.
    Portable,
    /// The per-user config folder of the OS.
    Platform,
}

impl ModsRule {
    pub fn label(self) -> &'static str {
        match self {
            ModsRule::Explicit => "given folder",
            ModsRule::ManagerEnv => "WWHD_MOD_MANAGER_DIR",
            ModsRule::UserDirEnv => "WWHD_USER_DIR",
            ModsRule::Portable => "portable mode",
            ModsRule::Platform => "user config folder",
        }
    }
}

/// The environment variables the recomp reads. An empty value counts as unset (the recomp's Windows build reads
/// `WWHD_USER_DIR` and `APPDATA` that way; for the others an empty path would be useless).
#[derive(Clone, Debug, Default)]
pub struct Env {
    pub manager_dir: Option<OsString>,
    pub user_dir: Option<OsString>,
    pub appdata: Option<OsString>,
    pub xdg_config: Option<OsString>,
    pub home: Option<OsString>,
}

impl Env {
    pub fn from_process() -> Env {
        Env {
            manager_dir: std::env::var_os("WWHD_MOD_MANAGER_DIR"),
            user_dir: std::env::var_os("WWHD_USER_DIR"),
            appdata: std::env::var_os("APPDATA"),
            xdg_config: std::env::var_os("XDG_CONFIG_HOME"),
            home: std::env::var_os("HOME"),
        }
    }
}

fn set(v: &Option<OsString>) -> Option<&OsString> {
    v.as_ref().filter(|s| !s.is_empty())
}

/// The game executable next to a disc dump: the recomp's `build/game` in the disc folder's parent, or in a
/// folder inside that parent. Only an existing file is returned.
pub fn game_exe_near_disc(disc_root: &Path) -> Option<PathBuf> {
    let parent = disc_root.parent()?;
    let name = if cfg!(windows) { "wwhd.exe" } else { "wwhd" };
    let mut dirs = vec![parent.to_path_buf()];
    if let Ok(rd) = std::fs::read_dir(parent) {
        dirs.extend(rd.flatten().map(|e| e.path()).filter(|p| p.is_dir()));
    }
    dirs.into_iter().map(|d| d.join("build").join("game").join(name)).find(|p| p.is_file())
}

/// The mod manager folder: `explicit` if given, else the recomp's order — `WWHD_MOD_MANAGER_DIR`,
/// `WWHD_USER_DIR`/ModManager, portable mode (`portable.txt` beside `game_exe` → `<its folder's parent>/user/ModManager`),
/// then the OS config folder (`%APPDATA%\WWHD`, `~/Library/Application Support/WWHD`, `$XDG_CONFIG_HOME/wwhd` or
/// `~/.config/wwhd`) with `ModManager` below. The folder may not exist; the caller decides what that means.
pub fn detect(explicit: Option<&Path>, game_exe: Option<&Path>, env: &Env) -> (PathBuf, ModsRule) {
    if let Some(p) = explicit {
        return (p.to_path_buf(), ModsRule::Explicit);
    }
    if let Some(d) = set(&env.manager_dir) {
        return (PathBuf::from(d), ModsRule::ManagerEnv);
    }
    if let Some(d) = set(&env.user_dir) {
        return (PathBuf::from(d).join("ModManager"), ModsRule::UserDirEnv);
    }
    if let Some(dir) = game_exe.and_then(Path::parent) {
        if dir.join("portable.txt").is_file() {
            if let Some(up) = dir.parent() {
                return (up.join("user").join("ModManager"), ModsRule::Portable);
            }
        }
    }
    let or_dot = |v: &Option<OsString>| PathBuf::from(set(v).cloned().unwrap_or_else(|| OsString::from(".")));
    let base = if cfg!(target_os = "macos") {
        or_dot(&env.home).join("Library/Application Support/WWHD")
    } else if cfg!(windows) {
        or_dot(&env.appdata).join("WWHD")
    } else if let Some(x) = set(&env.xdg_config) {
        PathBuf::from(x).join("wwhd")
    } else {
        or_dot(&env.home).join(".config/wwhd")
    };
    (base.join("ModManager"), ModsRule::Platform)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn os(s: &str) -> Option<OsString> {
        Some(OsString::from(s))
    }

    fn full_env() -> Env {
        Env { manager_dir: os("/m"), user_dir: os("/u"), appdata: os("/a"), xdg_config: os("/x"), home: os("/h") }
    }

    /// A temp "game install" with `bin/portable.txt`; returns its root and the exe path inside `bin`.
    fn portable_game(tag: &str) -> (PathBuf, PathBuf) {
        let root = std::env::temp_dir().join(format!("wwhd-modsdir-{tag}-{}", std::process::id()));
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("portable.txt"), b"").unwrap();
        (root, bin.join("wwhd.exe"))
    }

    #[test]
    fn explicit_wins_over_everything() {
        let (root, exe) = portable_game("explicit");
        let (p, r) = detect(Some(Path::new("/given")), Some(&exe), &full_env());
        assert_eq!((p, r), (PathBuf::from("/given"), ModsRule::Explicit));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn manager_env_wins_over_user_dir_portable_and_platform() {
        let (root, exe) = portable_game("manager");
        let (p, r) = detect(None, Some(&exe), &full_env());
        assert_eq!((p, r), (PathBuf::from("/m"), ModsRule::ManagerEnv));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn user_dir_wins_over_portable_and_platform() {
        let (root, exe) = portable_game("userdir");
        let env = Env { manager_dir: None, ..full_env() };
        let (p, r) = detect(None, Some(&exe), &env);
        assert_eq!((p, r), (PathBuf::from("/u").join("ModManager"), ModsRule::UserDirEnv));
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn empty_variables_count_as_unset() {
        let env = Env { manager_dir: os(""), user_dir: os(""), ..full_env() };
        assert_eq!(detect(None, None, &env).1, ModsRule::Platform);
    }

    #[test]
    fn portable_needs_marker_and_uses_user_folder_beside_the_exe_folder() {
        let (root, exe) = portable_game("portable");
        let env = Env { manager_dir: None, user_dir: None, ..full_env() };
        let (p, r) = detect(None, Some(&exe), &env);
        assert_eq!((p, r), (root.join("user").join("ModManager"), ModsRule::Portable));
        std::fs::remove_file(root.join("bin").join("portable.txt")).unwrap();
        assert_eq!(detect(None, Some(&exe), &env).1, ModsRule::Platform);
        assert_eq!(detect(None, None, &env).1, ModsRule::Platform);
        std::fs::remove_dir_all(root).ok();
    }

    #[cfg(windows)]
    #[test]
    fn windows_platform_rule_uses_appdata() {
        let env = Env { appdata: os(r"E:\Cache\Roaming"), ..Env::default() };
        let (p, r) = detect(None, None, &env);
        assert_eq!(r, ModsRule::Platform);
        assert_eq!(p, PathBuf::from(r"E:\Cache\Roaming").join("WWHD").join("ModManager"));
        assert!(p.to_string_lossy().ends_with(r"WWHD\ModManager"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_without_appdata_falls_back_to_current_dir_like_the_recomp() {
        let (p, _) = detect(None, None, &Env::default());
        assert_eq!(p, PathBuf::from(".").join("WWHD").join("ModManager"));
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    #[test]
    fn linux_platform_rule_uses_xdg_else_home_config() {
        let env = Env { xdg_config: os("/x"), home: os("/h"), ..Env::default() };
        assert_eq!(detect(None, None, &env), (PathBuf::from("/x/wwhd/ModManager"), ModsRule::Platform));
        let env = Env { xdg_config: None, home: os("/h"), ..Env::default() };
        assert_eq!(detect(None, None, &env), (PathBuf::from("/h/.config/wwhd/ModManager"), ModsRule::Platform));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_platform_rule_uses_application_support() {
        let env = Env { home: os("/h"), ..Env::default() };
        assert_eq!(detect(None, None, &env).0, PathBuf::from("/h/Library/Application Support/WWHD/ModManager"));
    }

    #[test]
    fn game_exe_is_found_in_build_game_next_to_the_disc() {
        let root = std::env::temp_dir().join(format!("wwhd-modsdir-exe-{}", std::process::id()));
        let game = root.join("Recomp").join("build").join("game");
        std::fs::create_dir_all(&game).unwrap();
        std::fs::create_dir_all(root.join("game_disc")).unwrap();
        let name = if cfg!(windows) { "wwhd.exe" } else { "wwhd" };
        assert_eq!(game_exe_near_disc(&root.join("game_disc")), None);
        std::fs::write(game.join(name), b"").unwrap();
        assert_eq!(game_exe_near_disc(&root.join("game_disc")), Some(game.join(name)));
        std::fs::remove_dir_all(root).ok();
    }
}
