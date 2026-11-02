//! Run the rewrite gate over every texture below a directory and print
//! statistics per format and tile mode. Usage: gate <disc content dir> [limit]

use std::collections::BTreeMap;
use std::path::Path;

use wwhd_tex::container::walk;
use wwhd_tex::{gx2, texture};

fn main() {
    let root = std::env::args().nth(1).expect("root dir");
    let limit: usize = std::env::args().nth(2).map(|s| s.parse().unwrap()).unwrap_or(usize::MAX);
    let mut files = Vec::new();
    collect(Path::new(&root), &mut files);
    files.sort();
    let mut stats: BTreeMap<String, [usize; 5]> = BTreeMap::new();
    let mut samples: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut others: BTreeMap<&'static str, usize> = BTreeMap::new();
    let errors = std::cell::Cell::new(0usize);
    for f in files.iter().take(limit) {
        let data = std::fs::read(f).unwrap();
        let rel = f.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/");
        walk(
            &rel,
            &data,
            &mut |loc, kind, leaf| match texture::list(&loc, kind, leaf) {
                Ok(list) => {
                    for t in list {
                        let key = format!(
                            "{:?} {} tm{} mips>1:{} dim{} depth{}",
                            kind,
                            gx2::format_name(t.surface.format),
                            t.surface.tile_mode,
                            t.surface.mip_count > 1,
                            t.surface.dim,
                            t.surface.depth
                        );
                        let g = texture::gate(&t, leaf);
                        let dec = texture::decode(&t, leaf).is_ok();
                        let e = stats.entry(key.clone()).or_default();
                        e[0] += 1;
                        e[1] += g.layout_matches as usize;
                        e[2] += g.regs_match as usize;
                        e[3] += g.roundtrip_exact as usize;
                        e[4] += dec as usize;
                        if !g.passed() {
                            let s = samples.entry(key).or_default();
                            if s.len() < 3 {
                                s.push(format!("{} {}x{} m{}: {}", t.id.key(), t.surface.width, t.surface.height, t.surface.mip_count, g.detail));
                            }
                        }
                    }
                }
                Err(e) => {
                    errors.set(errors.get() + 1);
                    eprintln!("{}: {e}", loc.display());
                }
            },
            &mut |_, kind, _| *others.entry(kind).or_default() += 1,
            &mut |loc, e| {
                errors.set(errors.get() + 1);
                eprintln!("{}: {e}", loc.display())
            },
        );
    }
    println!("format | count | layout ok | regs ok | roundtrip ok | decodes");
    for (k, v) in &stats {
        println!("{k} | {} | {} | {} | {} | {}", v[0], v[1], v[2], v[3], v[4]);
    }
    for (k, s) in &samples {
        println!("FAIL {k}");
        for x in s {
            println!("    {x}");
        }
    }
    println!("other leaves: {others:?}; errors: {}", errors.get());
}

fn collect(p: &Path, out: &mut Vec<std::path::PathBuf>) {
    for e in std::fs::read_dir(p).unwrap() {
        let e = e.unwrap();
        let path = e.path();
        if path.is_dir() {
            collect(&path, out);
        } else {
            out.push(path);
        }
    }
}
