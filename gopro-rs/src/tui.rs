//! Full-screen browser: list view, gallery grid, preview, import with progress.
use crate::camera::{already_have, fmt_dur, human, target_path, CamState, Kind, MediaFile, Source};
use crate::config;
use crate::player::{self, Player};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use image::DynamicImage;
use ratatui::{prelude::*, widgets::*};
use ratatui_image::{
    picker::{Picker, ProtocolType},
    protocol::Protocol,
    FilterType, FontSize, Image, Resize,
};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

const CARD_W: u16 = 30;
const CARD_H: u16 = 14;
const ACCENT: Color = Color::Rgb(255, 170, 0);
const DIM: Color = Color::DarkGray;

// ------------------------------------------------------------ background work
enum Job {
    Thumb(MediaFile, bool),
    Meta(MediaFile),
    Files,
    State,
    Delete(Vec<MediaFile>),
}

enum Reply {
    Thumb { path: String, big: bool, img: Option<DynamicImage> },
    Meta(String, Value),
    Files(std::result::Result<Vec<MediaFile>, String>),
    State(CamState),
    Deleted(usize, Vec<String>),
}

fn spawn_workers(src: Arc<Source>, n: usize) -> (Sender<Job>, Receiver<Reply>) {
    let (jtx, jrx) = mpsc::channel::<Job>();
    let jrx = Arc::new(Mutex::new(jrx));
    let (rtx, rrx) = mpsc::channel::<Reply>();
    for _ in 0..n {
        let (jrx, rtx, src) = (jrx.clone(), rtx.clone(), src.clone());
        thread::spawn(move || loop {
            let job = match jrx.lock().unwrap().recv() {
                Ok(j) => j,
                Err(_) => break,
            };
            let reply = match job {
                Job::Thumb(f, big) => Reply::Thumb { path: f.path.clone(), big, img: src.thumb(&f, big) },
                Job::Meta(f) => Reply::Meta(f.path.clone(), src.meta(&f)),
                Job::Files => Reply::Files(src.list().map_err(|e| e.to_string())),
                Job::State => Reply::State(src.state()),
                Job::Delete(files) => {
                    let mut ok = 0;
                    let mut errs = vec![];
                    for f in &files {
                        match src.delete(f) {
                            Ok(_) => ok += 1,
                            Err(e) => errs.push(format!("{}: {e}", f.name)),
                        }
                    }
                    Reply::Deleted(ok, errs)
                }
            };
            if rtx.send(reply).is_err() {
                break;
            }
        });
    }
    (jtx, rrx)
}

enum DlEvent {
    Begin { idx: usize, name: String, size: u64 },
    Progress { done: u64, moved: u64 },
    Log(String),
    FileEnd { counted: u64 },
    Finished { ok: usize, failed: usize, bytes: u64, secs: f64, cancelled: bool },
}

struct Download {
    total: u64,
    count: usize,
    idx: usize,
    cur_name: String,
    cur_size: u64,
    cur_done: u64,
    cur_moved: u64,
    cur_start: Instant,
    before: u64,
    log: Vec<String>,
    finished: bool,
    summary: String,
    cancel: Arc<AtomicBool>,
    rx: Receiver<DlEvent>,
}

fn start_download(src: Arc<Source>, files: Vec<MediaFile>, dest: PathBuf, flat: bool, streams: usize) -> Download {
    let (tx, rx) = mpsc::channel::<DlEvent>();
    let cancel = Arc::new(AtomicBool::new(false));
    let total: u64 = files.iter().map(|f| f.size).sum();
    let count = files.len();
    let c2 = cancel.clone();
    thread::spawn(move || {
        let _ka = src.camera().map(|c| {
            c.turbo(true);
            c.keepalive()
        });
        let t0 = Instant::now();
        let (mut ok, mut failed, mut bytes) = (0usize, 0usize, 0u64);
        let mut cancelled = false;
        for (i, f) in files.iter().enumerate() {
            if c2.load(Ordering::Relaxed) {
                cancelled = true;
                break;
            }
            let target = target_path(&dest, f, flat);
            let _ = tx.send(DlEvent::Begin { idx: i, name: format!("{} {}", f.icon(), f.name), size: f.size });
            if already_have(&target, f) {
                let _ = tx.send(DlEvent::Log(format!("skip   {}  (already imported)", f.name)));
                let _ = tx.send(DlEvent::FileEnd { counted: f.size });
                ok += 1;
                continue;
            }
            let mut last = Instant::now() - Duration::from_secs(1);
            let tx2 = tx.clone();
            let t1 = Instant::now();
            let r = src.fetch(f, &target, streams, &c2, &mut |done, _total, moved| {
                if last.elapsed() >= Duration::from_millis(100) {
                    last = Instant::now();
                    let _ = tx2.send(DlEvent::Progress { done, moved });
                }
            });
            match r {
                Ok(n) => {
                    ok += 1;
                    bytes += n;
                    let s = t1.elapsed().as_secs_f64().max(0.001);
                    let _ = tx.send(DlEvent::Log(format!("done   {}  {} in {:.0}s ({}/s)", f.name, human(n), s, human((n as f64 / s) as u64))));
                }
                Err(e) if e.to_string().contains("cancelled") => {
                    let _ = tx.send(DlEvent::Log(format!("paused {}  — partial kept, resumes next time", f.name)));
                    cancelled = true;
                    let _ = tx.send(DlEvent::FileEnd { counted: f.size });
                    break;
                }
                Err(e) => {
                    failed += 1;
                    let _ = tx.send(DlEvent::Log(format!("FAIL   {}: {e:#}", f.name)));
                }
            }
            let _ = tx.send(DlEvent::FileEnd { counted: f.size });
        }
        if let Some(c) = src.camera() {
            c.turbo(false);
        }
        let _ = tx.send(DlEvent::Finished { ok, failed, bytes, secs: t0.elapsed().as_secs_f64(), cancelled });
    });
    Download {
        total,
        count,
        idx: 0,
        cur_name: String::new(),
        cur_size: 0,
        cur_done: 0,
        cur_moved: 0,
        cur_start: Instant::now(),
        before: 0,
        log: vec![],
        finished: false,
        summary: String::new(),
        cancel,
        rx,
    }
}

// --------------------------------------------------------------------- app
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    List,
    Gallery,
}

enum Modal {
    Confirm(Vec<MediaFile>),
    Download(Download),
    Details(String),
    Play(Player),
    Help,
}

enum Entry {
    Day { label: String, count: usize, bytes: u64 },
    File(usize),
}

struct App {
    src: Arc<Source>,
    dest: PathBuf,
    flat: bool,
    label: String,
    files: Vec<MediaFile>,
    state: CamState,
    visible: Vec<usize>,
    rows: Vec<Entry>,
    file_rows: Vec<usize>,
    pos: usize,
    table: TableState,
    selected: HashSet<String>,
    kind_filter: u8,
    text_filter: String,
    filter_editing: bool,
    mode: Mode,
    gcols: usize,
    goff: usize,
    /// rows visible in the list / card rows visible in the gallery (for ctrl-d / ctrl-u)
    page_rows: usize,
    /// first `g` of a `gg` chord, waiting for the second
    pending_g: Option<Instant>,
    modal: Option<Modal>,
    thumbs: HashMap<(String, bool), Option<DynamicImage>>,
    requested: HashSet<(String, bool)>,
    metas: HashMap<String, Value>,
    meta_req: HashSet<String>,
    protos: HashMap<(String, bool, u16, u16), Protocol>,
    picker: Picker,
    jobs: Sender<Job>,
    replies: Receiver<Reply>,
    toast: Option<(String, Instant, bool)>,
    last_state: Instant,
    loading: bool,
    quit: bool,
    dest_editing: bool,
    dest_input: String,
    /// image rectangles drawn this frame / last frame (to know when a full repaint is needed)
    drawn_images: Vec<Rect>,
    prev_images: Vec<Rect>,
    force_clear: bool,
    streams: usize,
    ffmpeg: Option<PathBuf>,
    video_proto: Option<Protocol>,
    video_dirty: bool,
}

pub fn run(src: Source, dest: PathBuf, flat: bool, streams: usize) -> Result<()> {
    let picker = make_picker();
    let src = Arc::new(src);
    let (jobs, replies) = spawn_workers(src.clone(), 3);
    let mut app = App {
        label: src.label(),
        src,
        dest,
        flat,
        files: vec![],
        state: CamState::default(),
        visible: vec![],
        rows: vec![],
        file_rows: vec![],
        pos: 0,
        table: TableState::default(),
        selected: HashSet::new(),
        kind_filter: 0,
        text_filter: String::new(),
        filter_editing: false,
        mode: Mode::List,
        gcols: 4,
        goff: 0,
        page_rows: 10,
        pending_g: None,
        modal: None,
        thumbs: HashMap::new(),
        requested: HashSet::new(),
        metas: HashMap::new(),
        meta_req: HashSet::new(),
        protos: HashMap::new(),
        picker,
        jobs,
        replies,
        toast: None,
        last_state: Instant::now(),
        loading: true,
        quit: false,
        dest_editing: false,
        dest_input: String::new(),
        drawn_images: vec![],
        prev_images: vec![],
        force_clear: false,
        streams,
        ffmpeg: player::find_ffmpeg(),
        video_proto: None,
        video_dirty: false,
    };
    let _ = app.jobs.send(Job::Files);
    let _ = app.jobs.send(Job::State);

    let mut terminal = ratatui::init();
    let res = (|| -> Result<()> {
        while !app.quit {
            app.drawn_images.clear();
            terminal.draw(|f| app.draw(f))?;
            // Inline-image protocols (iTerm2/kitty/sixel) leave pixels behind when the cells
            // under them are "unchanged" text, so repaint from scratch whenever an image
            // region disappears or moves.
            let vanished = app.prev_images.iter().any(|r| !app.drawn_images.contains(r));
            if (vanished || app.force_clear) && app.picker.protocol_type() != ProtocolType::Halfblocks {
                app.force_clear = false;
                terminal.clear()?;
                app.drawn_images.clear();
                terminal.draw(|f| app.draw(f))?;
            }
            app.force_clear = false;
            app.prev_images = app.drawn_images.clone();
            if event::poll(Duration::from_millis(60))? {
                match event::read()? {
                    Event::Key(k) if k.kind != KeyEventKind::Release => app.key(k),
                    Event::Resize(_, _) => {
                        app.protos.clear();
                        app.force_clear = true;
                    }
                    _ => {}
                }
            }
            app.pump();
        }
        Ok(())
    })();
    ratatui::restore();
    res
}

impl App {
    // ---- data plumbing
    fn pump(&mut self) {
        while let Ok(r) = self.replies.try_recv() {
            match r {
                Reply::Thumb { path, big, img } => {
                    self.thumbs.insert((path, big), img);
                }
                Reply::Meta(p, v) => {
                    self.metas.insert(p, v);
                }
                Reply::State(s) => self.state = s,
                Reply::Files(Ok(files)) => {
                    self.loading = false;
                    let keep: HashSet<&String> = files.iter().map(|f| &f.path).collect();
                    self.selected.retain(|p| keep.contains(p));
                    let cur = self.cur().map(|f| f.path.clone());
                    self.files = files;
                    self.rebuild(cur.as_deref());
                    // prefetch small thumbs, current first
                    if let Some(f) = self.cur().cloned() {
                        self.want_thumb(&f, false);
                    }
                    for i in self.visible.clone() {
                        let f = self.files[i].clone();
                        self.want_thumb(&f, false);
                    }
                }
                Reply::Files(Err(e)) => {
                    self.loading = false;
                    self.toast(format!("could not list media: {e}"), true);
                }
                Reply::Deleted(n, errs) => {
                    self.selected.clear();
                    self.toast(format!("deleted {n} file(s) from camera"), false);
                    for e in errs {
                        self.toast(format!("delete failed: {e}"), true);
                    }
                    let _ = self.jobs.send(Job::Files);
                    let _ = self.jobs.send(Job::State);
                }
            }
        }
        if let Some(Modal::Play(p)) = &mut self.modal {
            if p.poll() {
                self.video_dirty = true;
            }
        }
        if let Some(Modal::Download(d)) = &mut self.modal {
            while let Ok(ev) = d.rx.try_recv() {
                match ev {
                    DlEvent::Begin { idx, name, size } => {
                        d.idx = idx;
                        d.cur_name = name;
                        d.cur_size = size;
                        d.cur_done = 0;
                        d.cur_moved = 0;
                        d.cur_start = Instant::now();
                    }
                    DlEvent::Progress { done, moved } => {
                        d.cur_done = done;
                        d.cur_moved = moved;
                    }
                    DlEvent::Log(s) => d.log.push(s),
                    DlEvent::FileEnd { counted } => {
                        d.before += counted;
                        d.cur_done = d.cur_size;
                    }
                    DlEvent::Finished { ok, failed, bytes, secs, cancelled } => {
                        d.finished = true;
                        d.summary = format!(
                            "Imported {ok}/{} · {} in {:.0}s ({}/s){}{}",
                            d.count,
                            human(bytes),
                            secs,
                            human((bytes as f64 / secs.max(0.001)) as u64),
                            if failed > 0 { format!(" · {failed} failed") } else { String::new() },
                            if cancelled { " · cancelled" } else { "" }
                        );
                    }
                }
            }
        }
        if let Some(t) = self.pending_g {
            if t.elapsed() > Duration::from_millis(400) {
                self.pending_g = None;
                self.toggle_gallery();
            }
        }
        if self.last_state.elapsed() > Duration::from_secs(30) && !self.importing() {
            self.last_state = Instant::now();
            let _ = self.jobs.send(Job::State);
        }
        if let Some((_, t, _)) = &self.toast {
            if t.elapsed() > Duration::from_secs(4) {
                self.toast = None;
            }
        }
    }

    fn toast(&mut self, msg: String, err: bool) {
        self.toast = Some((msg, Instant::now(), err));
    }

    fn importing(&self) -> bool {
        matches!(&self.modal, Some(Modal::Download(d)) if !d.finished)
    }

    fn want_thumb(&mut self, f: &MediaFile, big: bool) {
        if self.importing() && self.src.is_camera() {
            return; // the camera's server is saturated by the transfer; fetch later
        }
        let key = (f.path.clone(), big);
        if self.requested.insert(key) {
            let _ = self.jobs.send(Job::Thumb(f.clone(), big));
        }
    }

    fn want_meta(&mut self, f: &MediaFile) {
        if self.importing() {
            return;
        }
        if self.src.is_camera() && self.meta_req.insert(f.path.clone()) {
            let _ = self.jobs.send(Job::Meta(f.clone()));
        }
    }

    fn passes(&self, f: &MediaFile) -> bool {
        if f.is_sidecar() {
            return false;
        }
        match self.kind_filter {
            1 if f.kind != Kind::Video => return false,
            2 if f.kind != Kind::Photo => return false,
            _ => {}
        }
        self.text_filter.is_empty() || f.name.to_lowercase().contains(&self.text_filter.to_lowercase())
    }

    fn rebuild(&mut self, keep_path: Option<&str>) {
        self.visible = (0..self.files.len()).filter(|&i| self.passes(&self.files[i])).collect();
        self.rows.clear();
        self.file_rows.clear();
        let mut last = String::new();
        for &i in &self.visible {
            let f = &self.files[i];
            let day = f.day_long();
            if day != last {
                last = day.clone();
                let same: Vec<&MediaFile> = self.visible.iter().map(|&j| &self.files[j]).filter(|x| x.day_long() == day).collect();
                self.rows.push(Entry::Day { label: day, count: same.len(), bytes: same.iter().map(|x| x.size).sum() });
            }
            self.file_rows.push(self.rows.len());
            self.rows.push(Entry::File(i));
        }
        if let Some(p) = keep_path {
            if let Some(k) = self.visible.iter().position(|&i| self.files[i].path == p) {
                self.pos = k;
            }
        }
        self.pos = self.pos.min(self.visible.len().saturating_sub(1));
        self.sync_cursor();
    }

    fn sync_cursor(&mut self) {
        self.table.select(self.file_rows.get(self.pos).copied());
        if let Some(f) = self.cur().cloned() {
            self.want_thumb(&f, false);
            self.want_meta(&f);
        }
    }

    fn cur(&self) -> Option<&MediaFile> {
        self.visible.get(self.pos).map(|&i| &self.files[i])
    }

    fn move_by(&mut self, d: i64) {
        if self.visible.is_empty() {
            return;
        }
        let n = self.visible.len() as i64;
        self.pos = (self.pos as i64 + d).clamp(0, n - 1) as usize;
        self.sync_cursor();
    }

    fn jump_top(&mut self) {
        self.move_by(-(self.visible.len() as i64));
    }

    fn jump_bottom(&mut self) {
        self.move_by(self.visible.len() as i64);
    }

    /// vim ctrl-d / ctrl-u: half a screen (in cards for the gallery)
    fn half_page(&mut self, dir: i64) {
        let rows = (self.page_rows / 2).max(1) as i64;
        let step = if self.mode == Mode::Gallery { rows * self.gcols as i64 } else { rows };
        self.move_by(dir * step);
    }

    fn toggle_gallery(&mut self) {
        self.mode = if self.mode == Mode::List { Mode::Gallery } else { Mode::List };
    }

    fn selected_files(&self) -> Vec<MediaFile> {
        self.files.iter().filter(|f| self.selected.contains(&f.path)).cloned().collect()
    }

    fn toggle_current(&mut self) {
        if let Some(f) = self.cur() {
            let p = f.path.clone();
            if !self.selected.remove(&p) {
                self.selected.insert(p);
            }
        }
    }

    fn select_all_visible(&mut self) {
        let vis: HashSet<String> = self.visible.iter().map(|&i| self.files[i].path.clone()).collect();
        if vis.iter().all(|p| self.selected.contains(p)) {
            for p in &vis {
                self.selected.remove(p);
            }
        } else {
            self.selected.extend(vis);
        }
    }

    fn select_day(&mut self) {
        let Some(day) = self.cur().map(|f| f.day()) else { return };
        let same: Vec<String> = self.visible.iter().map(|&i| &self.files[i]).filter(|f| f.day() == day).map(|f| f.path.clone()).collect();
        if same.iter().all(|p| self.selected.contains(p)) {
            for p in &same {
                self.selected.remove(p);
            }
        } else {
            self.selected.extend(same);
        }
    }

    fn start_import(&mut self, files: Vec<MediaFile>) {
        if files.is_empty() {
            self.toast("Nothing selected — space selects, a selects all, s selects the day".into(), true);
            return;
        }
        let d = start_download(self.src.clone(), files, self.dest.clone(), self.flat, self.streams);
        self.modal = Some(Modal::Download(d));
    }

    fn meta_f64(&self, path: &str, key: &str) -> Option<f64> {
        let v = self.metas.get(path)?.get(key)?;
        v.as_f64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))
    }

    fn video_pixels(&self) -> (u32, u32) {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((120, 40));
        let fs = self.picker.font_size();
        let w = (cols.saturating_sub(4) as u32 * fs.width as u32).clamp(64, 1280) & !1;
        let h = (rows.saturating_sub(6) as u32 * fs.height as u32).clamp(36, 720) & !1;
        (w, h)
    }

    fn play_current(&mut self, external: bool) {
        let Some(f) = self.cur().cloned() else { return };
        if !player::is_video(&f) {
            self.want_thumb(&f, true);
            self.modal = Some(Modal::Details(f.path.clone()));
            return;
        }
        let target = player::playback_source(&self.src, &f, true);
        if external || self.ffmpeg.is_none() {
            let hint = if self.ffmpeg.is_none() { "  (brew install ffmpeg for in-terminal playback)" } else { "" };
            match player::open_external(&target) {
                Ok(app) => self.toast(format!("playing {} in {app}{hint}", f.name), false),
                Err(e) => self.toast(format!("could not open a player: {e}"), true),
            }
            return;
        }
        let dur = self.meta_f64(&f.path, "dur")
            .or_else(|| player::probe_duration(self.ffmpeg.as_ref().unwrap(), &target))
            .unwrap_or(0.0);
        let fps = if self.picker.protocol_type() == ProtocolType::Halfblocks { 10 } else { 12 };
        let (w, h) = self.video_pixels();
        let p = Player::new(self.ffmpeg.clone().unwrap(), target, f.name.clone(), dur, true, fps, w, h);
        self.video_proto = None;
        self.video_dirty = false;
        self.modal = Some(Modal::Play(p));
    }

    // ---- keys
    fn key(&mut self, k: KeyEvent) {
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            if let Some(Modal::Download(d)) = &self.modal {
                d.cancel.store(true, Ordering::Relaxed);
            }
            self.quit = true;
            return;
        }
        if self.modal.is_some() {
            return self.key_modal(k);
        }
        if self.dest_editing {
            match k.code {
                KeyCode::Esc => self.dest_editing = false,
                KeyCode::Enter => {
                    let p = config::expand(&self.dest_input);
                    if p.as_os_str().is_empty() {
                        self.toast("folder can't be empty".into(), true);
                    } else {
                        match std::fs::create_dir_all(&p) {
                            Ok(_) => {
                                self.dest = p.clone();
                                let _ = config::save_dest(&p);
                                self.toast(format!("imports now go to {}", p.display()), false);
                                self.dest_editing = false;
                            }
                            Err(e) => self.toast(format!("can't use {}: {e}", p.display()), true),
                        }
                    }
                }
                KeyCode::Backspace => {
                    self.dest_input.pop();
                }
                KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => self.dest_input.clear(),
                KeyCode::Char(c) => self.dest_input.push(c),
                _ => {}
            }
            return;
        }
        if self.filter_editing {
            match k.code {
                KeyCode::Esc => {
                    self.text_filter.clear();
                    self.filter_editing = false;
                }
                KeyCode::Enter => self.filter_editing = false,
                KeyCode::Backspace => {
                    self.text_filter.pop();
                }
                KeyCode::Char(c) => self.text_filter.push(c),
                _ => {}
            }
            let cur = self.cur().map(|f| f.path.clone());
            self.rebuild(cur.as_deref());
            return;
        }
        if k.modifiers.contains(KeyModifiers::CONTROL) {
            match k.code {
                KeyCode::Char('d') => self.half_page(1),
                KeyCode::Char('u') => self.half_page(-1),
                _ => {}
            }
            return;
        }
        // `gg` jumps to the top; a lone `g` (after a short pause) toggles the gallery
        if k.code == KeyCode::Char('g') {
            if self.pending_g.take().is_some() {
                self.jump_top();
            } else {
                self.pending_g = Some(Instant::now());
            }
            return;
        }
        if self.pending_g.take().is_some() {
            // `g` followed by something else: run the gallery toggle, then the key
            self.toggle_gallery();
        }
        match k.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Char('?') => self.modal = Some(Modal::Help),
            KeyCode::Tab => self.toggle_gallery(),
            KeyCode::Esc if self.mode == Mode::Gallery => self.mode = Mode::List,
            KeyCode::Esc if !self.text_filter.is_empty() => {
                self.text_filter.clear();
                let cur = self.cur().map(|f| f.path.clone());
                self.rebuild(cur.as_deref());
            }
            KeyCode::Char(' ') => {
                self.toggle_current();
                self.move_by(1);
            }
            KeyCode::Char('a') => self.select_all_visible(),
            KeyCode::Char('s') => self.select_day(),
            KeyCode::Char('d') => {
                let files = self.selected_files();
                self.start_import(files);
            }
            KeyCode::Char('D') => {
                let files: Vec<MediaFile> = self.visible.iter().map(|&i| self.files[i].clone()).collect();
                self.start_import(files);
            }
            KeyCode::Char('x') => {
                let files = self.selected_files();
                if files.is_empty() {
                    self.toast("Select files first (space / a / s)".into(), true);
                } else {
                    self.modal = Some(Modal::Confirm(files));
                }
            }
            KeyCode::Enter => {
                if let Some(f) = self.cur().cloned() {
                    self.want_thumb(&f, true);
                    self.want_meta(&f);
                    self.modal = Some(Modal::Details(f.path.clone()));
                }
            }
            KeyCode::Char('v') => {
                self.kind_filter = (self.kind_filter + 1) % 3;
                let cur = self.cur().map(|f| f.path.clone());
                self.rebuild(cur.as_deref());
            }
            KeyCode::Char('/') => self.filter_editing = true,
            KeyCode::Char('p') => self.play_current(false),
            KeyCode::Char('P') => self.play_current(true),
            KeyCode::Char('c') => {
                self.dest_input = self.dest.display().to_string();
                self.dest_editing = true;
            }
            KeyCode::Char('r') => {
                self.loading = true;
                let _ = self.jobs.send(Job::Files);
                let _ = self.jobs.send(Job::State);
            }
            KeyCode::Char('o') => {
                let _ = std::fs::create_dir_all(&self.dest);
                let _ = std::process::Command::new("open").arg(&self.dest).spawn();
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_by(if self.mode == Mode::Gallery { -(self.gcols as i64) } else { -1 }),
            KeyCode::Down | KeyCode::Char('j') => self.move_by(if self.mode == Mode::Gallery { self.gcols as i64 } else { 1 }),
            KeyCode::Left | KeyCode::Char('h') if self.mode == Mode::Gallery => self.move_by(-1),
            KeyCode::Right | KeyCode::Char('l') if self.mode == Mode::Gallery => self.move_by(1),
            KeyCode::PageUp => self.half_page(-2),
            KeyCode::PageDown => self.half_page(2),
            KeyCode::Home => self.jump_top(),
            KeyCode::End | KeyCode::Char('G') => self.jump_bottom(),
            _ => {}
        }
    }

    fn key_modal(&mut self, k: KeyEvent) {
        if matches!(self.modal, Some(Modal::Play(_))) {
            let Some(Modal::Play(mut p)) = self.modal.take() else { return };
            let mut keep = true;
            match k.code {
                KeyCode::Esc | KeyCode::Char('q') => keep = false,
                KeyCode::Char(' ') | KeyCode::Char('k') => p.toggle_pause(),
                KeyCode::Left | KeyCode::Char('h') => p.seek(-10.0),
                KeyCode::Right | KeyCode::Char('l') => p.seek(10.0),
                KeyCode::Down | KeyCode::Char('j') => p.seek(-60.0),
                KeyCode::Up => p.seek(60.0),
                KeyCode::Home | KeyCode::Char('r') => p.restart(),
                KeyCode::Char('f') => {
                    if let Some(f) = self.cur().cloned() {
                        let proxy = !p.proxy;
                        let src = player::playback_source(&self.src, &f, proxy);
                        p.switch_source(src, proxy);
                    }
                }
                KeyCode::Char('P') | KeyCode::Char('o') => {
                    let target = p.source.clone();
                    match player::open_external(&target) {
                        Ok(app) => self.toast(format!("opened in {app}"), false),
                        Err(e) => self.toast(format!("could not open a player: {e}"), true),
                    }
                }
                _ => {}
            }
            if keep {
                self.modal = Some(Modal::Play(p));
            } else {
                drop(p);
                self.video_proto = None;
            }
            return;
        }
        let close_and_refresh = |app: &mut App| {
            app.modal = None;
            app.loading = true;
            let _ = app.jobs.send(Job::Files);
            let _ = app.jobs.send(Job::State);
        };
        match self.modal.as_mut().unwrap() {
            Modal::Play(_) => {}
            Modal::Help | Modal::Details(_) => self.modal = None,
            Modal::Confirm(files) => match k.code {
                KeyCode::Char('y') | KeyCode::Enter => {
                    let files = std::mem::take(files);
                    self.modal = None;
                    self.toast(format!("deleting {} file(s)…", files.len()), false);
                    let _ = self.jobs.send(Job::Delete(files));
                }
                _ => self.modal = None,
            },
            Modal::Download(d) => {
                if d.finished {
                    if matches!(k.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') | KeyCode::Char(' ')) {
                        close_and_refresh(self);
                    }
                } else if matches!(k.code, KeyCode::Esc | KeyCode::Char('c') | KeyCode::Char('q')) {
                    d.cancel.store(true, Ordering::Relaxed);
                    d.log.push("cancelling after the current chunk…".into());
                }
            }
        }
    }

    // ---- drawing
    fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        let [header, status, body, footer] = Layout::vertical([Constraint::Length(1), Constraint::Length(1), Constraint::Min(5), Constraint::Length(1)]).areas(area);
        self.draw_header(f, header);
        self.draw_status(f, status);
        match self.mode {
            Mode::List => self.draw_list(f, body),
            Mode::Gallery => self.draw_gallery(f, body),
        }
        self.draw_footer(f, footer);
        if self.modal.is_some() {
            self.draw_modal(f, area);
        }
        if let Some((msg, _, err)) = &self.toast {
            let w = (msg.chars().count() as u16 + 4).min(area.width.saturating_sub(2));
            let r = Rect::new(area.width.saturating_sub(w + 1), 2, w, 3);
            f.render_widget(Clear, r);
            let style = if *err { Style::new().fg(Color::White).bg(Color::Red) } else { Style::new().fg(Color::Black).bg(ACCENT) };
            f.render_widget(Paragraph::new(msg.as_str()).style(style).block(Block::bordered().border_style(style)), r);
        }
    }

    fn draw_header(&self, f: &mut Frame, area: Rect) {
        let st = &self.state;
        let mut spans = vec![
            Span::styled(" ⬢ GoPro ", Style::new().fg(Color::Black).bg(ACCENT).bold()),
            Span::raw(" "),
            Span::styled(self.label.clone(), Style::new().bold()),
            Span::raw("   "),
        ];
        if let Some(b) = st.battery {
            let c = if b > 50 { Color::Green } else if b > 20 { Color::Yellow } else { Color::Red };
            spans.push(Span::raw("🔋 "));
            spans.push(Span::styled(format!("{} {b}%", bar(b as f64 / 100.0, 8)), Style::new().fg(c)));
            spans.push(Span::raw("   "));
        }
        if let Some(free) = st.free {
            spans.push(Span::raw(format!("💾 {} free   ", human(free))));
        }
        if st.recording {
            spans.push(Span::styled("● REC   ", Style::new().fg(Color::Red).bold()));
        }
        let vids = self.files.iter().filter(|x| x.kind == Kind::Video).count();
        let pics = self.files.iter().filter(|x| x.kind == Kind::Photo).count();
        spans.push(Span::styled(format!("🎬 {vids} · 📷 {pics} · {}", human(self.files.iter().map(|x| x.size).sum())), Style::new().fg(DIM)));
        if self.loading {
            spans.push(Span::styled("   ⟳ loading…", Style::new().fg(ACCENT)));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn draw_status(&self, f: &mut Frame, area: Rect) {
        let n = self.selected.len();
        let size: u64 = self.files.iter().filter(|x| self.selected.contains(&x.path)).map(|x| x.size).sum();
        let mut spans = vec![Span::raw(" ")];
        if n > 0 {
            spans.push(Span::styled(format!("✔ {n} selected · {}", human(size)), Style::new().fg(Color::Green).bold()));
        } else {
            spans.push(Span::styled("nothing selected", Style::new().fg(DIM)));
        }
        let mut filt = vec![];
        if self.kind_filter == 1 {
            filt.push("videos only".to_string());
        }
        if self.kind_filter == 2 {
            filt.push("photos only".to_string());
        }
        if self.filter_editing {
            filt.push(format!("name: {}▏", self.text_filter));
        } else if !self.text_filter.is_empty() {
            filt.push(format!("name: '{}'", self.text_filter));
        }
        if !filt.is_empty() {
            spans.push(Span::raw("   │   "));
            spans.push(Span::styled(format!("filter: {}", filt.join(", ")), Style::new().fg(Color::Cyan)));
        }
        spans.push(Span::raw("   │   "));
        spans.push(Span::styled(
            format!("{} {}/{}", if self.mode == Mode::Gallery { "gallery" } else { "list" }, if self.visible.is_empty() { 0 } else { self.pos + 1 }, self.visible.len()),
            Style::new().fg(DIM),
        ));
        spans.push(Span::raw("   │   "));
        if self.dest_editing {
            spans.push(Span::styled("import folder: ", Style::new().fg(ACCENT).bold()));
            spans.push(Span::styled(format!("{}▏", self.dest_input), Style::new().fg(Color::White).bold()));
            spans.push(Span::styled("   enter save · esc cancel · ctrl-u clear", Style::new().fg(DIM)));
        } else {
            spans.push(Span::styled("→ ", Style::new().fg(DIM)));
            spans.push(Span::styled(self.dest.display().to_string(), Style::new().fg(Color::White)));
            spans.push(Span::styled("  (c to change, o to open)", Style::new().fg(DIM)));
        }
        f.render_widget(Paragraph::new(Line::from(spans)).style(Style::new().bg(Color::Rgb(30, 30, 40))), area);
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        let keys: &[(&str, &str)] = if self.dest_editing {
            &[("type", "folder path"), ("enter", "save"), ("esc", "cancel")]
        } else if self.filter_editing {
            &[("type", "filter"), ("enter", "apply"), ("esc", "clear")]
        } else if self.mode == Mode::Gallery {
            &[("←↑↓→", "move"), ("space", "select"), ("a", "all"), ("d", "import"), ("enter", "details"), ("p", "play"), ("esc/g", "list"), ("?", "help"), ("q", "quit")]
        } else {
            &[("↑↓", "move"), ("space", "select"), ("a", "all"), ("s", "day"), ("d", "import"), ("g", "gallery"), ("enter", "details"), ("p", "play"), ("v", "type"), ("/", "find"), ("c", "folder"), ("x", "delete"), ("?", "help"), ("q", "quit")]
        };
        let mut spans = vec![];
        for (k, v) in keys {
            spans.push(Span::styled(format!(" {k} "), Style::new().fg(Color::Black).bg(Color::Gray).bold()));
            spans.push(Span::styled(format!(" {v}  "), Style::new().fg(DIM)));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn draw_list(&mut self, f: &mut Frame, area: Rect) {
        let (tbl, pv) = if area.width >= 100 {
            let [a, b] = Layout::horizontal([Constraint::Min(50), Constraint::Length(46)]).areas(area);
            (a, Some(b))
        } else {
            (area, None)
        };
        self.page_rows = tbl.height.saturating_sub(3).max(1) as usize;
        let mut rows = Vec::with_capacity(self.rows.len());
        for e in &self.rows {
            match e {
                Entry::Day { label, count, bytes } => rows.push(
                    Row::new(vec![
                        Cell::from(""),
                        Cell::from(Span::styled(format!("▍{label}"), Style::new().fg(Color::Cyan).bold())),
                        Cell::from(""),
                        Cell::from(Line::from(Span::styled(human(*bytes), Style::new().fg(Color::Cyan))).right_aligned()),
                        Cell::from(Span::styled(format!("{count} files"), Style::new().fg(Color::Cyan))),
                        Cell::from(""),
                    ])
                    .style(Style::new().bg(Color::Rgb(20, 35, 45))),
                ),
                Entry::File(i) => {
                    let x = &self.files[*i];
                    let sel = self.selected.contains(&x.path);
                    let mark = if sel { Span::styled("✔", Style::new().fg(Color::Green).bold()) } else { Span::styled("·", Style::new().fg(DIM)) };
                    let name = Span::styled(format!("{} {}", x.icon(), x.name), if sel { Style::new().bold() } else { Style::new() });
                    let kind = Span::styled(x.kind.label(), Style::new().fg(if x.kind == Kind::Video { Color::Magenta } else { Color::Yellow }));
                    let size = Line::from(format!("{}{}", human(x.size), if x.size_estimated { "~" } else { "" })).right_aligned();
                    rows.push(Row::new(vec![
                        Cell::from(mark),
                        Cell::from(name),
                        Cell::from(kind),
                        Cell::from(size),
                        Cell::from(x.time()),
                        Cell::from(Span::styled(x.note(), Style::new().fg(DIM))),
                    ]));
                }
            }
        }
        let widths = [Constraint::Length(2), Constraint::Length(18), Constraint::Length(6), Constraint::Length(11), Constraint::Length(9), Constraint::Min(8)];
        let title = if self.visible.is_empty() && !self.loading { " Media — nothing matches " } else { " Media " };
        let table = Table::new(rows, widths)
            .header(Row::new(vec!["", "file", "kind", "size", "shot at", "note"]).style(Style::new().fg(DIM).underlined()))
            .block(Block::bordered().title(title).border_style(Style::new().fg(DIM)))
            .row_highlight_style(Style::new().bg(Color::Rgb(60, 50, 20)).bold())
            .highlight_symbol("▶ ");
        f.render_stateful_widget(table, tbl, &mut self.table);
        if let Some(pv) = pv {
            self.draw_preview(f, pv);
        }
    }

    fn render_image(&mut self, f: &mut Frame, path: &str, big: bool, area: Rect, placeholder: &str) {
        self.render_image_inner(f, path, big, area, placeholder, false)
    }

    fn render_image_inner(&mut self, f: &mut Frame, path: &str, big: bool, area: Rect, placeholder: &str, in_modal: bool) {
        if area.width < 4 || area.height < 2 {
            return;
        }
        // never paint images underneath an open modal: they would show through it
        if self.modal.is_some() && !in_modal {
            return;
        }
        match self.thumbs.get(&(path.to_string(), big)) {
            Some(Some(img)) => {
                let key = (path.to_string(), big, area.width, area.height);
                if !self.protos.contains_key(&key) {
                    if self.protos.len() > 600 {
                        self.protos.clear();
                    }
                    // upscale small thumbnails so they fill the available cells
                    let fs = self.picker.font_size();
                    let (tw, th) = (area.width as f64 * fs.width as f64, area.height as f64 * fs.height as f64);
                    let scale = (tw / img.width() as f64).min(th / img.height() as f64);
                    let src_img = if scale > 1.0 {
                        img.resize((img.width() as f64 * scale) as u32, (img.height() as f64 * scale) as u32, FilterType::Triangle)
                    } else {
                        img.clone()
                    };
                    if let Ok(p) = self.picker.new_protocol(src_img, Size::new(area.width, area.height), Resize::Fit(Some(FilterType::Triangle))) {
                        self.protos.insert(key.clone(), p);
                    }
                }
                if let Some(p) = self.protos.get(&key) {
                    let sz = p.size();
                    let x = area.x + area.width.saturating_sub(sz.width) / 2;
                    let y = area.y + area.height.saturating_sub(sz.height) / 2;
                    let r = Rect::new(x, y, sz.width.min(area.width), sz.height.min(area.height));
                    self.drawn_images.push(r);
                    f.render_widget(Image::new(p), r);
                }
            }
            Some(None) => f.render_widget(Paragraph::new(format!("\n{placeholder}\nno thumbnail")).centered().style(Style::new().fg(DIM)), area),
            None => f.render_widget(Paragraph::new(format!("\n{placeholder}\nloading…")).centered().style(Style::new().fg(DIM)), area),
        }
    }

    fn meta_lines(&self, x: &MediaFile) -> Vec<Line<'static>> {
        let mut rows: Vec<(&str, String)> = vec![];
        if let Some(c) = x.created {
            rows.push(("Shot", c.format("%a %d %b %Y %H:%M:%S").to_string()));
        }
        rows.push(("Size", format!("{}{}", human(x.size), if x.size_estimated { " (approx)" } else { "" })));
        if let Some(m) = self.metas.get(&x.path) {
            let g = |k: &str| m.get(k).and_then(|v| v.as_str().map(|s| s.to_string()).or_else(|| v.as_u64().map(|n| n.to_string())));
            if let (Some(w), Some(h)) = (g("w"), g("h")) {
                rows.push(("Frame", format!("{w}×{h}")));
            }
            if let Some(d) = g("dur").and_then(|s| s.parse::<f64>().ok()) {
                let fps = match (g("fps").and_then(|s| s.parse::<f64>().ok()), g("fps_denom").and_then(|s| s.parse::<f64>().ok())) {
                    (Some(n), Some(d)) if d > 0.0 => format!(" @ {:.3} fps", n / d).replace(".000", ""),
                    _ => String::new(),
                };
                rows.push(("Length", format!("{}{fps}", fmt_dur(d))));
            }
            if g("eis").as_deref() == Some("1") {
                rows.push(("Stabilised", "HyperSmooth on".into()));
            }
            if g("hdr").as_deref() == Some("1") {
                rows.push(("HDR", "on".into()));
            }
            if let Some(a) = g("ao") {
                rows.push(("Audio", a));
            }
            if let Some(ls) = g("ls").and_then(|s| s.parse::<u64>().ok()) {
                if ls > 0 {
                    rows.push(("Proxy LRV", human(ls)));
                }
            }
        } else if self.src.is_camera() {
            rows.push(("Details", "loading…".into()));
        }
        if !x.note().is_empty() {
            rows.push(("Note", x.note()));
        }
        rows.push(("On card", x.path.clone()));
        let w = rows.iter().map(|(k, _)| k.len()).max().unwrap_or(6);
        rows.into_iter()
            .map(|(k, v)| Line::from(vec![Span::styled(format!("{k:>w$}  "), Style::new().fg(ACCENT).bold()), Span::raw(v)]))
            .collect()
    }

    fn draw_preview(&mut self, f: &mut Frame, area: Rect) {
        let Some(x) = self.cur().cloned() else {
            f.render_widget(Paragraph::new("Select a file to preview").style(Style::new().fg(DIM)).block(Block::bordered().border_style(Style::new().fg(DIM))), area);
            return;
        };
        let block = Block::bordered().title(format!(" {} {} ", x.icon(), x.name)).border_style(Style::new().fg(ACCENT));
        let inner = block.inner(area);
        f.render_widget(block, area);
        let img_h = (inner.height / 2).clamp(6, 16);
        let [img, meta] = Layout::vertical([Constraint::Length(img_h), Constraint::Min(3)]).areas(inner);
        self.render_image(f, &x.path, false, img, x.icon());
        let lines = self.meta_lines(&x);
        f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), Rect::new(meta.x, meta.y + 1, meta.width, meta.height.saturating_sub(1)));
    }

    fn draw_gallery(&mut self, f: &mut Frame, area: Rect) {
        let block = Block::bordered().title(" Gallery ").border_style(Style::new().fg(DIM));
        let inner = block.inner(area);
        f.render_widget(block, area);
        if self.visible.is_empty() {
            f.render_widget(Paragraph::new(if self.loading { "loading…" } else { "nothing to show" }).centered().style(Style::new().fg(DIM)), inner);
            return;
        }
        let cols = ((inner.width) / CARD_W).max(1) as usize;
        let vis_rows = (inner.height / CARD_H).max(1) as usize;
        self.gcols = cols;
        self.page_rows = vis_rows;
        let cur_row = self.pos / cols;
        if cur_row < self.goff {
            self.goff = cur_row;
        } else if cur_row >= self.goff + vis_rows {
            self.goff = cur_row + 1 - vis_rows;
        }
        let first = self.goff * cols;
        let last = (first + vis_rows * cols).min(self.visible.len());
        let pad_x = (inner.width - (cols as u16) * CARD_W) / 2;
        for k in first..last {
            let x = self.files[self.visible[k]].clone();
            self.want_thumb(&x, false);
            let col = (k - first) % cols;
            let row = (k - first) / cols;
            let r = Rect::new(inner.x + pad_x + col as u16 * CARD_W, inner.y + row as u16 * CARD_H, CARD_W, CARD_H);
            let focused = k == self.pos;
            let sel = self.selected.contains(&x.path);
            let border = if focused {
                Style::new().fg(ACCENT).bold()
            } else if sel {
                Style::new().fg(Color::Green)
            } else {
                Style::new().fg(DIM)
            };
            let mut card = Block::bordered().border_style(border);
            if sel {
                card = card.title(Span::styled(" ✔ ", Style::new().fg(Color::Green).bold()));
            }
            if focused {
                card = card.border_type(BorderType::Thick);
            }
            let ci = card.inner(r);
            f.render_widget(card, r);
            let [img, cap] = Layout::vertical([Constraint::Min(4), Constraint::Length(2)]).areas(ci);
            self.render_image(f, &x.path, false, img, x.icon());
            let when = x.created.map(|d| d.format("%d %b %H:%M").to_string()).unwrap_or_else(|| "?".into());
            let cap_lines = vec![
                Line::from(Span::styled(format!("{} {}", x.icon(), x.name), if focused || sel { Style::new().bold() } else { Style::new() })),
                Line::from(Span::styled(format!("{when} · {}", human(x.size)), Style::new().fg(DIM))),
            ];
            f.render_widget(Paragraph::new(cap_lines), cap);
        }
        // scroll hint
        let total_rows = (self.visible.len() + cols - 1) / cols;
        if total_rows > vis_rows {
            let txt = format!(" rows {}–{} of {total_rows} ", self.goff + 1, (self.goff + vis_rows).min(total_rows));
            let w = txt.len() as u16;
            f.render_widget(Paragraph::new(txt).style(Style::new().fg(DIM)), Rect::new(area.x + area.width.saturating_sub(w + 2), area.y, w, 1));
        }
    }

    fn draw_modal(&mut self, f: &mut Frame, area: Rect) {
        let modal = self.modal.take().unwrap();
        match &modal {
            Modal::Play(p) => {
                let r = centered(area, area.width.saturating_sub(2), area.height.saturating_sub(1));
                f.render_widget(Clear, r);
                let mode = if p.proxy { "proxy" } else { "full quality" };
                let block = Block::bordered()
                    .title(format!(" ▶ {}  ·  {mode}  ·  {}×{} @ {} fps ", p.title, p.w, p.h, p.fps))
                    .border_style(Style::new().fg(ACCENT));
                let inner = block.inner(r);
                f.render_widget(block, r);
                let [vid, bar, hint] = Layout::vertical([Constraint::Min(4), Constraint::Length(1), Constraint::Length(1)]).areas(inner);
                if let Some(err) = &p.error {
                    f.render_widget(Paragraph::new(format!("\n\nplayback failed:\n{err}")).centered().style(Style::new().fg(Color::Red)), vid);
                } else {
                    if (self.video_dirty || self.video_proto.is_none()) && p.frame.is_some() {
                        if let Ok(pr) = self.picker.new_protocol(p.frame.clone().unwrap(), Size::new(vid.width, vid.height), Resize::Fit(None)) {
                            self.video_proto = Some(pr);
                        }
                        self.video_dirty = false;
                    }
                    match &self.video_proto {
                        Some(pr) => {
                            let sz = pr.size();
                            let x = vid.x + vid.width.saturating_sub(sz.width) / 2;
                            let y = vid.y + vid.height.saturating_sub(sz.height) / 2;
                            let rr = Rect::new(x, y, sz.width.min(vid.width), sz.height.min(vid.height));
                            self.drawn_images.push(rr);
                            f.render_widget(Image::new(pr), rr);
                        }
                        None => f.render_widget(Paragraph::new("\n\nbuffering…").centered().style(Style::new().fg(DIM)), vid),
                    }
                }
                let pos = p.position();
                let label = if p.duration > 0.0 { format!("{}{} / {}", if p.paused { "⏸ " } else { "" }, fmt_dur(pos), fmt_dur(p.duration)) } else { format!("{}{}", if p.paused { "⏸ " } else { "" }, fmt_dur(pos)) };
                let ratio = if p.duration > 0.0 { (pos / p.duration).clamp(0.0, 1.0) } else { 0.0 };
                let gstyle = if p.ended { Style::new().fg(Color::Green).bg(Color::Rgb(50, 50, 60)) } else { Style::new().fg(ACCENT).bg(Color::Rgb(50, 50, 60)) };
                f.render_widget(Gauge::default().ratio(ratio).gauge_style(gstyle).label(if p.ended { format!("{label}  ·  end — r to replay") } else { label }), bar);
                f.render_widget(
                    Paragraph::new("space pause   ←/→ 10s   ↑/↓ 60s   r restart   f proxy/full   P open with sound   esc close   (no audio in terminal)").style(Style::new().fg(DIM)),
                    hint,
                );
            }
            Modal::Help => {
                let r = centered(area, 66, 24);
                f.render_widget(Clear, r);
                let lines = [
                    ("↑↓ / jk", "move          "), ("^d / ^u", "half a page down / up (PgUp/PgDn: a page)"),
                    ("gg / G", "jump to top / bottom"),
                    ("space", "select / unselect"), ("a", "select all (toggle)"),
                    ("s", "select everything shot that day"), ("d", "import selected"),
                    ("D", "import everything shown"), ("g / tab", "gallery ⇄ list"),
                    ("enter", "details + large preview"), ("p / P", "play video here / in VLC-QuickTime"), ("v", "cycle videos / photos / all"),
                    ("/", "filter by name"), ("x", "delete selected from camera"),
                    ("r", "refresh"), ("c", "change the import folder (remembered)"), ("o", "open import folder in Finder"),
                    ("q", "quit"),
                ];
                let text: Vec<Line> = lines.iter().map(|(k, v)| Line::from(vec![Span::styled(format!("  {k:>10}  "), Style::new().fg(ACCENT).bold()), Span::raw(*v)])).collect();
                f.render_widget(Paragraph::new(text).block(Block::bordered().title(" Keys ").border_style(Style::new().fg(ACCENT))), r);
            }
            Modal::Confirm(files) => {
                let r = centered(area, 64, (files.len().min(12) + 7) as u16);
                f.render_widget(Clear, r);
                let mut text = vec![Line::from(format!("{} file(s), {} will be erased from the camera's SD card.", files.len(), human(files.iter().map(|x| x.size).sum()))), Line::from("")];
                for x in files.iter().take(12) {
                    text.push(Line::from(format!("   {} {}", x.icon(), x.name)));
                }
                if files.len() > 12 {
                    text.push(Line::from("   …"));
                }
                text.push(Line::from(""));
                text.push(Line::from(vec![Span::styled(" y ", Style::new().fg(Color::Black).bg(Color::Red).bold()), Span::raw(" delete     "), Span::styled(" n ", Style::new().fg(Color::Black).bg(Color::Gray).bold()), Span::raw(" cancel")]));
                f.render_widget(Paragraph::new(text).block(Block::bordered().title(" Delete from camera? ").border_style(Style::new().fg(Color::Red))), r);
            }
            Modal::Details(path) => {
                let r = centered(area, area.width.saturating_sub(8).min(110), area.height.saturating_sub(4).min(40));
                f.render_widget(Clear, r);
                if let Some(x) = self.files.iter().find(|x| &x.path == path).cloned() {
                    let block = Block::bordered().title(format!(" {} {} ", x.icon(), x.name)).border_style(Style::new().fg(ACCENT));
                    let inner = block.inner(r);
                    f.render_widget(block, r);
                    let meta = self.meta_lines(&x);
                    let mh = (meta.len() as u16).min(inner.height / 3);
                    let [img, m] = Layout::vertical([Constraint::Min(4), Constraint::Length(mh)]).areas(inner);
                    let have_big = matches!(self.thumbs.get(&(path.clone(), true)), Some(Some(_)));
                    self.render_image_inner(f, path, have_big, img, x.icon(), true);
                    f.render_widget(Paragraph::new(meta).wrap(Wrap { trim: false }), m);
                }
            }
            Modal::Download(d) => {
                let r = centered(area, area.width.saturating_sub(6).min(90), 22.min(area.height.saturating_sub(2)));
                f.render_widget(Clear, r);
                let block = Block::bordered()
                    .title(format!(" Importing {} files · {} → {} ", d.count, human(d.total), self.dest.display()))
                    .border_style(Style::new().fg(if d.finished { Color::Green } else { ACCENT }));
                let inner = block.inner(r);
                f.render_widget(block, r);
                let [l1, g1, l2, g2, log, hint] = Layout::vertical([
                    Constraint::Length(1), Constraint::Length(1), Constraint::Length(1), Constraint::Length(1), Constraint::Min(3), Constraint::Length(1),
                ]).areas(inner);
                let secs = d.cur_start.elapsed().as_secs_f64().max(0.001);
                let rate = d.cur_moved as f64 / secs;
                let eta = if rate > 1.0 && d.cur_size > d.cur_done { fmt_dur((d.cur_size - d.cur_done) as f64 / rate) } else { "–".into() };
                let cur = if d.finished { d.summary.clone() } else { format!("[{}/{}] {}   {}/s   eta {eta}", d.idx + 1, d.count, d.cur_name, human(rate as u64)) };
                f.render_widget(Paragraph::new(cur).style(Style::new().bold()), l1);
                let fr = if d.cur_size > 0 { (d.cur_done as f64 / d.cur_size as f64).min(1.0) } else { 0.0 };
                f.render_widget(Gauge::default().ratio(fr).gauge_style(Style::new().fg(ACCENT).bg(Color::Rgb(50, 50, 60))).label(format!("{} / {}", human(d.cur_done), human(d.cur_size))), g1);
                let overall_done = (d.before + d.cur_done.min(d.cur_size)).min(d.total);
                let orf = if d.total > 0 { overall_done as f64 / d.total as f64 } else { 1.0 };
                f.render_widget(Paragraph::new(format!("overall  {} / {}", human(overall_done), human(d.total))).style(Style::new().fg(DIM)), l2);
                f.render_widget(Gauge::default().ratio(orf).gauge_style(Style::new().fg(Color::Green).bg(Color::Rgb(50, 50, 60))).label(format!("{:.0}%", orf * 100.0)), g2);
                let n = log.height as usize;
                let start = d.log.len().saturating_sub(n);
                let lines: Vec<Line> = d.log[start..]
                    .iter()
                    .map(|s| {
                        let style = if s.starts_with("done") { Style::new().fg(Color::Green) } else if s.starts_with("FAIL") { Style::new().fg(Color::Red) } else if s.starts_with("skip") { Style::new().fg(DIM) } else { Style::new().fg(Color::Yellow) };
                        Line::from(Span::styled(s.clone(), style))
                    })
                    .collect();
                f.render_widget(Paragraph::new(lines).block(Block::new().borders(Borders::TOP).border_style(Style::new().fg(DIM))), log);
                let h = if d.finished { "enter / esc  close" } else { "esc  cancel (partial files are kept and resume next time)" };
                f.render_widget(Paragraph::new(h).style(Style::new().fg(DIM)), hint);
            }
        }
        self.modal = Some(modal);
    }
}

/// Pick an image protocol without the stdin capability query.
///
/// The query spawns a thread that reads stdin and restores the terminal mode when it finally
/// gets a byte — in terminals that never answer (tmux, some multiplexers) that byte is the
/// user's first keystroke, which then silently drops the TUI out of raw mode. So we decide from
/// the environment instead; `GOPRO_IMG=query` opts back into the query, and
/// `GOPRO_IMG=halfblocks|sixel|kitty|iterm2` forces a protocol.
fn make_picker() -> Picker {
    let want = std::env::var("GOPRO_IMG").unwrap_or_default().to_lowercase();
    if want == "query" {
        return Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
    }
    let font = crossterm::terminal::window_size()
        .ok()
        .filter(|w| w.columns > 0 && w.rows > 0 && w.width > 0 && w.height > 0)
        .map(|w| FontSize { width: (w.width / w.columns).max(1), height: (w.height / w.rows).max(1) })
        .unwrap_or(FontSize { width: 8, height: 16 });
    let mut picker = Picker::from_fontsize(font);
    let env = |k: &str| std::env::var(k).unwrap_or_default().to_lowercase();
    let in_tmux = std::env::var_os("TMUX").is_some();
    let term_program = env("TERM_PROGRAM");
    let term = env("TERM");
    let proto = match want.as_str() {
        "halfblocks" | "half" | "blocks" | "unicode" => ProtocolType::Halfblocks,
        "sixel" => ProtocolType::Sixel,
        "kitty" | "tgp" => ProtocolType::Kitty,
        "iterm" | "iterm2" => ProtocolType::Iterm2,
        _ if in_tmux => ProtocolType::Halfblocks,
        _ if term_program == "iterm.app" || env("LC_TERMINAL") == "iterm2" || term_program == "wezterm" || term_program == "mintty" => ProtocolType::Iterm2,
        _ if term.contains("kitty") || std::env::var_os("KITTY_WINDOW_ID").is_some() || term_program == "ghostty" || term.contains("ghostty") => ProtocolType::Kitty,
        _ => ProtocolType::Halfblocks,
    };
    picker.set_protocol_type(proto);
    picker
}

fn bar(frac: f64, width: usize) -> String {
    let n = (frac.clamp(0.0, 1.0) * width as f64).round() as usize;
    format!("{}{}", "▰".repeat(n), "▱".repeat(width - n))
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h)
}
