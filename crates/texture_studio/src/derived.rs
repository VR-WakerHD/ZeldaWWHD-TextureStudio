//! Numbers the views show every frame but that only change with the policy, the
//! inventory or the project's files: estimates, the upscaled-file cache and the
//! mods folder check. They are worked out once (or on a worker thread) and read
//! from here, so a frame does no filesystem work and no per-texture arithmetic.

use std::collections::BTreeMap;
use std::path::PathBuf;

use wwhd_tex::classify::Class;
use wwhd_tex::gx2;
use wwhd_tex::inventory::Inventory;
use wwhd_tex::mips;
use wwhd_tex::mods_dir::{Env, ModsRule};
use wwhd_tex::pipeline;
use wwhd_tex::policy::{self, Decisions, Plan, ReviewStatus};
use wwhd_tex::project::{Project, ScaleChoice, Settings};
use wwhd_tex::runtime_pack::{self, DDS_HEADER_BYTES};

/// What one class (or all of them) costs at the current policy.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClassEstimate {
    pub unique: usize,
    /// Unique textures the rebuilt-archive plan upscales.
    pub upscaled: usize,
    /// GX2 bytes of those textures before and after (rebuilt archives).
    pub bytes_now: u64,
    pub bytes_after: u64,
    /// Unique textures that get a runtime-pack entry.
    pub replaced: usize,
    /// Level-0 texels of those entries at the planned scale.
    pub texels: u64,
    /// DDS file bytes (level data plus headers) and the level data alone (texture memory if all were resident).
    pub disk: u64,
    pub vram: u64,
}

impl ClassEstimate {
    fn add(&mut self, o: &ClassEstimate) {
        self.unique += o.unique;
        self.upscaled += o.upscaled;
        self.bytes_now += o.bytes_now;
        self.bytes_after += o.bytes_after;
        self.replaced += o.replaced;
        self.texels += o.texels;
        self.disk += o.disk;
        self.vram += o.vram;
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Estimate {
    pub classes: BTreeMap<Class, ClassEstimate>,
    pub total: ClassEstimate,
}

/// Per-class and total estimates for the unique textures `unique`. `plans` are the rebuilt-archive plans and
/// `rt_plans` the runtime ones (index-aligned with `inv.textures`); the runtime numbers follow the export's rules for
/// which textures get an entry (format, 2D, selected files, review state) and count one entry per unique key.
pub fn estimate(inv: &Inventory, unique: &[usize], plans: &[Plan], rt_plans: &[Plan], d: &Decisions, s: &Settings) -> Estimate {
    let mut est = Estimate::default();
    for &i in unique {
        let t = &inv.textures[i];
        let class = d.classification(t).class;
        let e = est.classes.entry(class).or_default();
        e.unique += 1;
        if plans[i].upscales() {
            e.upscaled += 1;
            e.bytes_now += policy::surface_bytes(t, 1);
            e.bytes_after += policy::surface_bytes(t, plans[i].scale);
        }
        let rt = &rt_plans[i];
        if !rt.upscales() || !s.file_selected(&t.id.location.file) || t.surface.dim != gx2::DIM_2D || t.surface.depth > 1 {
            continue;
        }
        if d.review(&t.key).status == ReviewStatus::Pending && !s.export_unreviewed {
            continue;
        }
        let Some(fmt) = runtime_pack::pack_format(t.surface.format, class) else { continue };
        let (w, h) = (t.surface.width * rt.scale, t.surface.height * rt.scale);
        let levels = mips::upscaled_mip_count(t.surface.mip_count, rt.scale).min(runtime_pack::max_levels(w, h));
        let data = fmt.data_bytes(w, h, levels);
        e.replaced += 1;
        e.texels += w as u64 * h as u64;
        e.vram += data;
        e.disk += data + DDS_HEADER_BYTES;
    }
    let mut total = ClassEstimate::default();
    for e in est.classes.values() {
        total.add(e);
    }
    est.total = total;
    est
}

/// A cached upscale: the PNG and the scale it really has.
pub type Found = Option<(PathBuf, u32)>;

/// What the project's files say, worked out by `scan_upscaled`.
#[derive(Clone, Debug, Default)]
pub struct ScanResult {
    /// The cached upscale of every planned texture, index-aligned with `inv.textures` (`None` for duplicates, native
    /// textures and textures not upscaled yet).
    pub found: Vec<Found>,
    /// Unique textures whose lossless master exists.
    pub extracted: usize,
    /// Bytes below the project folder (cache, masters, output).
    pub disk_bytes: u64,
}

/// Look at the project's files for the unique textures `unique`. Does filesystem work for every texture: run it off
/// the UI thread.
pub fn scan_upscaled(p: &Project, inv: &Inventory, unique: &[usize], plans: &[Plan]) -> ScanResult {
    let mut found: Vec<Found> = vec![None; inv.textures.len()];
    let mut extracted = 0;
    for &i in unique {
        let pl = &plans[i];
        if pl.upscales() {
            found[i] = pipeline::find_upscaled(p, &pl.backend.key(), &inv.textures[i].key, pl.scale);
        }
        if p.master_path(&inv.textures[i].key).exists() {
            extracted += 1;
        }
    }
    ScanResult { found, extracted, disk_bytes: dir_size(&p.root) }
}

/// Bytes of all files below `p`.
pub fn dir_size(p: &std::path::Path) -> u64 {
    let mut total = 0;
    if let Ok(rd) = std::fs::read_dir(p) {
        for e in rd.flatten() {
            let path = e.path();
            if path.is_dir() {
                total += dir_size(&path);
            } else if let Ok(m) = e.metadata() {
                total += m.len();
            }
        }
    }
    total
}

/// Where the upscaled-file scan stands. Only one scan runs at a time; a change that arrives meanwhile queues one more.
#[derive(Default)]
pub struct FoundCache {
    found: Vec<Found>,
    pub extracted: usize,
    pub disk_bytes: u64,
    in_flight: bool,
    pending: bool,
}

impl FoundCache {
    /// Something that decides which files exist changed. True when the caller should start a scan now.
    pub fn invalidate(&mut self) -> bool {
        if self.in_flight {
            self.pending = true;
            false
        } else {
            self.in_flight = true;
            true
        }
    }
    /// A scan finished with `found`. True when another scan is due because a change came in while it ran.
    pub fn finish(&mut self, r: ScanResult) -> bool {
        self.found = r.found;
        self.extracted = r.extracted;
        self.disk_bytes = r.disk_bytes;
        if self.pending {
            self.pending = false;
            true
        } else {
            self.in_flight = false;
            false
        }
    }
    /// Forget everything (no project or inventory); a scan still running is rescanned when it ends.
    pub fn clear(&mut self) {
        self.found.clear();
        self.extracted = 0;
        self.pending = self.in_flight;
    }
    pub fn get(&self, i: usize) -> Option<&(PathBuf, u32)> {
        self.found.get(i)?.as_ref()
    }
}

/// Set a class's scale in place (keeping the list order), or append it.
pub fn set_class_scale(list: &mut Vec<(Class, ScaleChoice)>, class: Class, choice: ScaleChoice) {
    match list.iter_mut().find(|(c, _)| *c == class) {
        Some(e) => e.1 = choice,
        None => list.push((class, choice)),
    }
}

/// What the Export and Settings views show about the folders outside the project; refreshed when the settings or the
/// view change, never per frame.
#[derive(Clone, Debug)]
pub struct EnvInfo {
    pub mods_path: PathBuf,
    pub mods_rule: ModsRule,
    pub mods_exists: bool,
    pub has_profiles: bool,
    pub game_exe: Option<PathBuf>,
    /// The disc folder holds a `content` folder (empty = not chosen).
    pub disc_ok: bool,
}

impl EnvInfo {
    pub fn detect(s: &Settings, env: &Env) -> EnvInfo {
        let (mods_path, mods_rule) = crate::cli::detect_mods_root(None, s, env);
        EnvInfo {
            mods_exists: mods_path.is_dir(),
            has_profiles: mods_path.join("profiles.json").is_file(),
            game_exe: crate::cli::game_exe(s),
            disc_ok: !s.disc_root.as_os_str().is_empty() && s.disc_root.join("content").is_dir(),
            mods_path,
            mods_rule,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wwhd_tex::classify::Classification;
    use wwhd_tex::container::AssetLocation;
    use wwhd_tex::gx2::Gx2Surface;
    use wwhd_tex::inventory::TextureRecord;
    use wwhd_tex::texture::TextureId;

    fn tex(key: &str, side: u32, format: u32, class: Class) -> TextureRecord {
        TextureRecord {
            id: TextureId { location: AssetLocation { file: "a.szs".into(), inner: vec![] }, index: 0, name: key.into() },
            kind: wwhd_tex::container::LeafKind::Bfres,
            surface: Gx2Surface { dim: 1, width: side, height: side, depth: 1, mip_count: 1, format, ..Default::default() },
            comp_sel: [0, 1, 2, 3],
            data_hash: key.into(),
            key: key.into(),
            gate_ok: true,
            gate_detail: String::new(),
            decodable: true,
            classification: Classification { class, confidence: 1.0, reason: String::new(), manual: false },
        }
    }

    fn setup(textures: Vec<TextureRecord>, s: &Settings) -> (Inventory, Vec<usize>, Vec<Plan>, Vec<Plan>) {
        let inv = Inventory { textures, ..Inventory::default() };
        let d = Decisions::default();
        let (plans, rt) = (policy::plan_all(&inv, s, &d), policy::plan_all_runtime(&inv, s, &d));
        let unique = (0..inv.textures.len()).collect();
        (inv, unique, plans, rt)
    }

    fn s_with(scale: u32) -> Settings {
        Settings { scale, exclude_files: vec![], ..Settings::default() }
    }

    #[test]
    fn estimate_follows_the_policy() {
        // One 128 px BC1 texture: BC7 in the pack, 1 byte per texel.
        let t = tex("k1", 128, 0x31, Class::Rock);
        let d = Decisions::default();
        let s4 = s_with(4);
        let (inv, unique, plans, rt) = setup(vec![t.clone()], &s4);
        let e4 = estimate(&inv, &unique, &plans, &rt, &d, &s4);
        let rock = &e4.classes[&Class::Rock];
        assert_eq!((rock.unique, rock.replaced, rock.texels), (1, 1, 512 * 512));
        // a single-level source stays single-level (the export's mip rule)
        assert_eq!(rock.vram, 512 * 512);
        assert_eq!(rock.disk, rock.vram + DDS_HEADER_BYTES);
        assert_eq!(e4.total, *rock);
        // a higher global scale raises every number; max side 4096 still allows x32 on 128 px
        let s32 = s_with(32);
        let (inv, unique, plans, rt) = setup(vec![t], &s32);
        let e32 = estimate(&inv, &unique, &plans, &rt, &d, &s32);
        assert!(e32.total.texels > e4.total.texels && e32.total.vram > e4.total.vram && e32.total.disk > e4.total.disk);
        assert_eq!(e32.total.texels, 4096 * 4096);
    }

    #[test]
    fn estimate_skips_native_unsupported_and_unselected_textures() {
        let s = s_with(4);
        let mut ramp_class = tex("k2", 128, 0x31, Class::Ramp);
        ramp_class.classification.class = Class::Ramp;
        let unsupported = tex("k3", 128, 0x08, Class::Rock);
        let (inv, unique, plans, rt) = setup(vec![ramp_class, unsupported], &s);
        let e = estimate(&inv, &unique, &plans, &rt, &Decisions::default(), &s);
        assert_eq!(e.total.unique, 2);
        assert_eq!((e.total.replaced, e.total.vram), (0, 0));
        let only = Settings { only_files: vec!["other".into()], ..s_with(4) };
        let (inv, unique, plans, rt) = setup(vec![tex("k1", 128, 0x31, Class::Rock)], &only);
        assert_eq!(estimate(&inv, &unique, &plans, &rt, &Decisions::default(), &only).total.replaced, 0);
    }

    #[test]
    fn found_cache_runs_one_scan_and_queues_one_more() {
        let mut c = FoundCache::default();
        assert!(c.invalidate(), "first change starts a scan");
        assert!(!c.invalidate(), "a scan is running: queue");
        assert!(!c.invalidate(), "queued once, however many changes");
        let r = |found: Vec<Found>| ScanResult { found, ..ScanResult::default() };
        assert!(c.finish(r(vec![None])), "the queued change needs another scan");
        assert!(!c.finish(r(vec![Some((PathBuf::from("x.png"), 4))])), "nothing queued: done");
        assert_eq!(c.get(0), Some(&(PathBuf::from("x.png"), 4)));
        assert_eq!(c.get(7), None);
        assert!(c.invalidate(), "idle again");
        c.clear();
        assert_eq!(c.get(0), None);
        assert!(c.finish(r(vec![])), "a clear during a scan asks for a rescan");
    }

    #[test]
    fn scan_finds_existing_upscales_only() {
        let root = std::env::temp_dir().join(format!("wwhd-derived-{}", std::process::id()));
        let p = Project::open(&root).unwrap();
        let s = s_with(4);
        let (inv, unique, _, plans) = setup(vec![tex("k1", 128, 0x31, Class::Rock), tex("k2", 128, 0x31, Class::Rock)], &s);
        assert!(plans[0].upscales());
        let path = p.upscaled_path(&plans[0].backend.key(), "k1", plans[0].scale);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"png").unwrap();
        let r = scan_upscaled(&p, &inv, &unique, &plans);
        assert_eq!(r.found[0], Some((path, 4)));
        assert_eq!(r.found[1], None);
        assert_eq!(r.extracted, 0);
        assert!(r.disk_bytes >= 3);
        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn class_scale_is_replaced_in_place() {
        let mut l = vec![(Class::Rock, ScaleChoice::Global), (Class::Ramp, ScaleChoice::Native)];
        set_class_scale(&mut l, Class::Rock, ScaleChoice::X(8));
        assert_eq!(l, vec![(Class::Rock, ScaleChoice::X(8)), (Class::Ramp, ScaleChoice::Native)]);
        set_class_scale(&mut l, Class::Ui, ScaleChoice::X(2));
        assert_eq!(l.len(), 3);
    }

    #[test]
    fn env_info_reports_the_detected_folder_and_the_disc_check() {
        let root = std::env::temp_dir().join(format!("wwhd-envinfo-{}", std::process::id()));
        let (mods, disc) = (root.join("ModManager"), root.join("disc"));
        std::fs::create_dir_all(mods.join("Mods")).unwrap();
        std::fs::create_dir_all(disc.join("content")).unwrap();
        let env = Env { manager_dir: Some(mods.clone().into_os_string()), ..Env::default() };
        let mut s = Settings { disc_root: disc.clone(), ..Settings::default() };
        let i = EnvInfo::detect(&s, &env);
        assert_eq!((i.mods_path.clone(), i.mods_rule, i.mods_exists, i.has_profiles, i.disc_ok), (mods.clone(), ModsRule::ManagerEnv, true, false, true));
        std::fs::write(mods.join("profiles.json"), b"{}").unwrap();
        s.mods_root = Some(root.join("manual"));
        s.disc_root = root.join("empty");
        let i = EnvInfo::detect(&s, &env);
        assert_eq!((i.mods_rule, i.mods_exists, i.disc_ok), (ModsRule::Explicit, false, false));
        assert!(EnvInfo::detect(&Settings { mods_root: None, ..s }, &env).has_profiles);
        std::fs::remove_dir_all(root).ok();
    }
}
