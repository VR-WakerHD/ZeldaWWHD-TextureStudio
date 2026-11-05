//! Background PNG loading and a bounded GPU texture cache for the GUI.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};

use eframe::egui::{self, ColorImage, TextureHandle, TextureOptions};

/// Cache key: the path, plus the channel map when it is not the identity.
fn keyed(path: &PathBuf, map: [u8; 4]) -> PathBuf {
    if map == [0, 1, 2, 3] {
        path.clone()
    } else {
        PathBuf::from(format!("{}#{}{}{}{}", path.display(), map[0], map[1], map[2], map[3]))
    }
}

pub struct ImageCache {
    textures: HashMap<PathBuf, TextureHandle>,
    order: VecDeque<PathBuf>,
    pending: HashSet<PathBuf>,
    failed: HashSet<PathBuf>,
    tx: Sender<(PathBuf, [u8; 4])>,
    rx: Receiver<(PathBuf, Option<ColorImage>)>,
    capacity: usize,
}

impl ImageCache {
    pub fn new(ctx: &egui::Context, capacity: usize) -> ImageCache {
        let (tx, req_rx) = channel::<(PathBuf, [u8; 4])>();
        let (res_tx, rx) = channel();
        let req_rx = std::sync::Arc::new(std::sync::Mutex::new(req_rx));
        for _ in 0..4 {
            let req_rx = req_rx.clone();
            let res_tx = res_tx.clone();
            let ctx = ctx.clone();
            std::thread::spawn(move || loop {
                let (path, map) = match req_rx.lock().unwrap().recv() {
                    Ok(p) => p,
                    Err(_) => return,
                };
                let img = image::open(&path).ok().map(|i| {
                    let mut rgba = i.to_rgba8();
                    wwhd_tex::pipeline::apply_display_map(&mut rgba, map);
                    ColorImage::from_rgba_unmultiplied([rgba.width() as usize, rgba.height() as usize], rgba.as_raw())
                });
                if res_tx.send((keyed(&path, map), img)).is_err() {
                    return;
                }
                ctx.request_repaint();
            });
        }
        ImageCache { textures: HashMap::new(), order: VecDeque::new(), pending: HashSet::new(), failed: HashSet::new(), tx, rx, capacity }
    }

    /// Upload finished loads. Call once per frame.
    pub fn pump(&mut self, ctx: &egui::Context) {
        while let Ok((path, img)) = self.rx.try_recv() {
            self.pending.remove(&path);
            match img {
                Some(img) => {
                    let opts = TextureOptions { magnification: egui::TextureFilter::Nearest, mipmap_mode: Some(egui::TextureFilter::Linear), ..TextureOptions::LINEAR };
                    let h = ctx.load_texture(path.to_string_lossy(), img, opts);
                    self.textures.insert(path.clone(), h);
                    self.order.push_back(path);
                    while self.order.len() > self.capacity {
                        if let Some(old) = self.order.pop_front() {
                            self.textures.remove(&old);
                        }
                    }
                }
                None => {
                    self.failed.insert(path);
                }
            }
        }
    }

    /// The texture if loaded; otherwise queues it and returns None.
    pub fn get(&mut self, path: &PathBuf) -> Option<TextureHandle> {
        self.get_mapped(path, [0, 1, 2, 3])
    }

    /// Like `get`, with stored channels remapped for display (see `display_map`).
    pub fn get_mapped(&mut self, path: &PathBuf, map: [u8; 4]) -> Option<TextureHandle> {
        let key = keyed(path, map);
        if let Some(t) = self.textures.get(&key) {
            return Some(t.clone());
        }
        if !self.pending.contains(&key) && !self.failed.contains(&key) && path.exists() {
            self.pending.insert(key);
            let _ = self.tx.send((path.clone(), map));
        }
        None
    }

    /// Forget a path (all channel mappings) so a changed file is reloaded.
    pub fn invalidate(&mut self, path: &PathBuf) {
        let prefix = path.to_string_lossy().into_owned();
        let stale = |k: &PathBuf| k.to_string_lossy().starts_with(&prefix);
        self.textures.retain(|k, _| !stale(k));
        self.failed.retain(|k| !stale(k));
        self.order.retain(|k| !stale(k));
    }
}
