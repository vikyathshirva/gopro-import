//! Video preview: in-terminal playback via ffmpeg (frames piped as raw RGB), plus
//! hand-off to an external player with sound.
use crate::camera::{Kind, MediaFile, Source};
use anyhow::Result;
use image::{DynamicImage, RgbImage};
use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{
        mpsc::{self, Receiver, TryRecvError},
        Arc, Mutex,
    },
    thread,
};

pub fn find_ffmpeg() -> Option<PathBuf> {
    for p in ["/opt/homebrew/bin/ffmpeg", "/usr/local/bin/ffmpeg", "/usr/bin/ffmpeg"] {
        if Path::new(p).exists() {
            return Some(PathBuf::from(p));
        }
    }
    std::env::var_os("PATH").and_then(|paths| std::env::split_paths(&paths).map(|d| d.join("ffmpeg")).find(|p| p.exists()))
}

fn which(bin: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|paths| std::env::split_paths(&paths).map(|d| d.join(bin)).find(|p| p.exists()))
}

/// URL (camera) or local path (SD card) to play; `proxy` prefers the small .LRV file.
pub fn playback_source(src: &Source, f: &MediaFile, proxy: bool) -> String {
    match src {
        Source::Camera { cam, .. } => {
            if proxy && f.lrv_size > 0 {
                if let Some(p) = f.lrv_path() {
                    return cam.media_url(&p);
                }
            }
            cam.media_url(&f.path)
        }
        Source::Sd(_) => {
            let local = f.local.clone().unwrap_or_default();
            if proxy {
                if let Some(n) = f.lrv_name() {
                    let p = local.with_file_name(n);
                    if p.exists() {
                        return p.display().to_string();
                    }
                }
            }
            local.display().to_string()
        }
    }
}

/// Open in the best available player with audio. Returns the player's name.
pub fn open_external(target: &str) -> Result<String> {
    if let Some(mpv) = which("mpv") {
        Command::new(mpv).arg(target).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
        return Ok("mpv".into());
    }
    for (path, name) in [
        ("/Applications/IINA.app", "IINA"),
        ("/Applications/VLC.app", "VLC"),
        ("/System/Applications/QuickTime Player.app", "QuickTime Player"),
    ] {
        if Path::new(path).exists() {
            Command::new("open").args(["-a", name, target]).status()?;
            return Ok(name.into());
        }
    }
    Command::new("open").arg(target).status()?;
    Ok("default app".into())
}

/// Duration in seconds via ffprobe (sits next to ffmpeg), if available.
pub fn probe_duration(ffmpeg: &Path, source: &str) -> Option<f64> {
    let ffprobe = ffmpeg.with_file_name("ffprobe");
    if !ffprobe.exists() {
        return None;
    }
    let out = Command::new(ffprobe)
        .args(["-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0", source])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse::<f64>().ok().filter(|d| *d > 0.0)
}

pub fn is_video(f: &MediaFile) -> bool {
    f.kind == Kind::Video
}

pub struct Player {
    ffmpeg: PathBuf,
    pub source: String,
    pub title: String,
    pub duration: f64,
    pub proxy: bool,
    pub fps: u32,
    pub w: u32,
    pub h: u32,
    child: Option<Child>,
    rx: Option<Receiver<Vec<u8>>>,
    stderr: Arc<Mutex<String>>,
    pub start_pos: f64,
    pub frames: u64,
    pub paused: bool,
    pub ended: bool,
    pub error: Option<String>,
    pub frame: Option<DynamicImage>,
}

impl Player {
    #[allow(clippy::too_many_arguments)]
    pub fn new(ffmpeg: PathBuf, source: String, title: String, duration: f64, proxy: bool, fps: u32, w: u32, h: u32) -> Player {
        let mut p = Player {
            ffmpeg,
            source,
            title,
            duration,
            proxy,
            fps: fps.max(1),
            w: w.max(2) & !1,
            h: h.max(2) & !1,
            child: None,
            rx: None,
            stderr: Arc::new(Mutex::new(String::new())),
            start_pos: 0.0,
            frames: 0,
            paused: false,
            ended: false,
            error: None,
            frame: None,
        };
        p.spawn();
        p
    }

    fn spawn(&mut self) {
        self.stop();
        self.frames = 0;
        self.ended = false;
        self.error = None;
        self.paused = false;
        let vf = format!(
            "fps={fps},scale={w}:{h}:force_original_aspect_ratio=decrease,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2:black",
            fps = self.fps,
            w = self.w,
            h = self.h
        );
        let mut cmd = Command::new(&self.ffmpeg);
        cmd.args(["-hide_banner", "-loglevel", "error", "-nostdin", "-re"]);
        if self.start_pos > 0.05 {
            cmd.args(["-ss", &format!("{:.2}", self.start_pos)]);
        }
        cmd.args(["-i", &self.source, "-an", "-sn", "-vf", &vf, "-f", "rawvideo", "-pix_fmt", "rgb24", "pipe:1"]);
        cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        match cmd.spawn() {
            Err(e) => self.error = Some(format!("can't start ffmpeg: {e}")),
            Ok(mut child) => {
                let mut stdout = child.stdout.take().unwrap();
                let mut stderr = child.stderr.take().unwrap();
                let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(3);
                let frame_len = (self.w * self.h * 3) as usize;
                thread::spawn(move || loop {
                    let mut buf = vec![0u8; frame_len];
                    if stdout.read_exact(&mut buf).is_err() || tx.send(buf).is_err() {
                        break;
                    }
                });
                let errbuf = self.stderr.clone();
                *errbuf.lock().unwrap() = String::new();
                thread::spawn(move || {
                    let mut s = String::new();
                    let _ = stderr.read_to_string(&mut s);
                    *errbuf.lock().unwrap() = s;
                });
                self.child = Some(child);
                self.rx = Some(rx);
            }
        }
    }

    /// Drain decoded frames; keeps the newest. Returns true if the picture changed.
    pub fn poll(&mut self) -> bool {
        let mut got = None;
        let mut disconnected = false;
        if let Some(rx) = &self.rx {
            loop {
                match rx.try_recv() {
                    Ok(b) => {
                        got = Some(b);
                        self.frames += 1;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
        if disconnected {
            self.rx = None;
            self.ended = true;
            if let Some(mut c) = self.child.take() {
                let _ = c.wait();
            }
            if self.frames == 0 && self.error.is_none() {
                let e = self.stderr.lock().unwrap().trim().to_string();
                self.error = Some(if e.is_empty() { "ffmpeg produced no frames".into() } else { e.lines().last().unwrap_or("").to_string() });
            }
        }
        if let Some(b) = got {
            if let Some(img) = RgbImage::from_raw(self.w, self.h, b) {
                self.frame = Some(DynamicImage::ImageRgb8(img));
                return true;
            }
        }
        false
    }

    pub fn position(&self) -> f64 {
        self.start_pos + self.frames as f64 / self.fps as f64
    }

    pub fn toggle_pause(&mut self) {
        if let Some(c) = &self.child {
            let sig = if self.paused { "-CONT" } else { "-STOP" };
            let _ = Command::new("kill").arg(sig).arg(c.id().to_string()).status();
            self.paused = !self.paused;
        }
    }

    pub fn seek_to(&mut self, t: f64) {
        let mut t = t.max(0.0);
        if self.duration > 0.0 {
            t = t.min((self.duration - 0.5).max(0.0));
        }
        self.start_pos = t;
        self.spawn();
    }

    pub fn seek(&mut self, delta: f64) {
        self.seek_to(self.position() + delta);
    }

    pub fn restart(&mut self) {
        self.seek_to(0.0);
    }

    pub fn switch_source(&mut self, source: String, proxy: bool) {
        let pos = self.position();
        self.source = source;
        self.proxy = proxy;
        self.seek_to(pos);
    }

    pub fn stop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = Command::new("kill").arg("-CONT").arg(c.id().to_string()).status();
            let _ = c.kill();
            let _ = c.wait();
        }
        self.rx = None;
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop();
    }
}
