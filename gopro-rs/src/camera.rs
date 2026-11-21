//! GoPro media model, discovery, Open GoPro HTTP client, SD-card fallback.
use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Local, TimeZone};
use image::DynamicImage;
use regex::Regex;
use serde_json::Value;
use std::{
    fs::{self, File, OpenOptions},
    sync::Mutex,
    io::{Read, Write},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, OnceLock,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

pub const WIFI_IP: &str = "10.5.5.9";
const CHUNK: usize = 1 << 20;

// ------------------------------------------------------------------ model
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Video,
    Photo,
    Other,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Video => "video",
            Kind::Photo => "photo",
            Kind::Other => "other",
        }
    }
}

#[derive(Clone, Debug)]
pub struct MediaFile {
    pub folder: String,
    pub name: String,
    /// `100GOPRO/GX010001.MP4`
    pub path: String,
    pub size: u64,
    pub size_estimated: bool,
    pub created: Option<DateTime<Local>>,
    pub kind: Kind,
    pub chapter: Option<u32>,
    pub number: Option<u32>,
    pub group_type: Option<String>,
    pub ext: String,
    pub lrv_size: u64,
    /// set when the source is a mounted SD card
    pub local: Option<PathBuf>,
}

impl MediaFile {
    pub fn is_sidecar(&self) -> bool {
        matches!(self.ext.as_str(), ".LRV" | ".THM")
    }
    pub fn day(&self) -> String {
        self.created.map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_else(|| "undated".into())
    }
    pub fn day_long(&self) -> String {
        self.created.map(|d| d.format("%a %d %b %Y").to_string()).unwrap_or_else(|| "undated".into())
    }
    pub fn time(&self) -> String {
        self.created.map(|d| d.format("%H:%M:%S").to_string()).unwrap_or_else(|| "?".into())
    }
    pub fn icon(&self) -> &'static str {
        match self.kind {
            Kind::Video => {
                if self.chapter.unwrap_or(1) > 1 {
                    "🎥"
                } else {
                    "🎬"
                }
            }
            Kind::Photo => match self.group_type.as_deref() {
                Some("b") => "💥",
                Some("t") => "⏳",
                Some("n") => "🌙",
                Some("c") => "📸",
                _ => "📷",
            },
            Kind::Other => "📄",
        }
    }
    pub fn note(&self) -> String {
        if self.kind == Kind::Video && self.chapter.unwrap_or(1) > 1 {
            return format!("chapter {} of #{:04}", self.chapter.unwrap(), self.number.unwrap_or(0));
        }
        match self.group_type.as_deref() {
            Some("b") => "burst".into(),
            Some("c") => "continuous".into(),
            Some("n") => "night lapse".into(),
            Some("t") => "time lapse".into(),
            Some(_) => "group".into(),
            None => String::new(),
        }
    }
    /// Low-res proxy name (GX010124.MP4 → GL010124.LRV) for videos.
    pub fn lrv_name(&self) -> Option<String> {
        if self.kind != Kind::Video || self.name.len() < self.ext.len() {
            return None;
        }
        let stem = &self.name[..self.name.len() - self.ext.len()];
        let up = stem.to_uppercase();
        if (up.starts_with("GX") || up.starts_with("GH")) && stem.len() > 2 {
            Some(format!("GL{}.LRV", &stem[2..]))
        } else {
            Some(format!("{stem}.LRV"))
        }
    }
    pub fn lrv_path(&self) -> Option<String> {
        self.lrv_name().map(|n| format!("{}/{}", self.folder, n))
    }
    pub fn matches(&self, names: &[String]) -> bool {
        if names.is_empty() {
            return true;
        }
        let up = self.name.to_uppercase();
        let stem = up.rsplit_once('.').map(|(s, _)| s.to_string()).unwrap_or(up.clone());
        names.iter().any(|n| {
            let n = n.to_uppercase();
            n == up || n == stem
        })
    }
}

fn re(s: &'static str, cell: &'static OnceLock<Regex>) -> &'static Regex {
    cell.get_or_init(|| Regex::new(s).unwrap())
}

pub fn parse_name(name: &str) -> (Kind, Option<u32>, Option<u32>, String) {
    static R1: OnceLock<Regex> = OnceLock::new();
    static R2: OnceLock<Regex> = OnceLock::new();
    static R3: OnceLock<Regex> = OnceLock::new();
    let (stem, ext) = match name.rfind('.') {
        Some(i) => (&name[..i], name[i..].to_uppercase()),
        None => (name, String::new()),
    };
    if let Some(c) = re(r"(?i)^(GX|GH|GL|GP|GS)(\d{2})(\d{4})$", &R1).captures(stem) {
        return (Kind::Video, c[2].parse().ok(), c[3].parse().ok(), ext);
    }
    if let Some(c) = re(r"(?i)^GOPR(\d{4})$", &R2).captures(stem) {
        let k = if ext == ".MP4" { Kind::Video } else { Kind::Photo };
        return (k, Some(1), c[1].parse().ok(), ext);
    }
    if let Some(c) = re(r"(?i)^G(\d{3})(\d{4})$", &R3).captures(stem) {
        return (Kind::Photo, c[1].parse().ok(), c[2].parse().ok(), ext);
    }
    let k = match ext.as_str() {
        ".JPG" | ".GPR" | ".HEIC" => Kind::Photo,
        ".MP4" | ".360" => Kind::Video,
        _ => Kind::Other,
    };
    (k, None, None, ext)
}

pub fn human(n: u64) -> String {
    let mut v = n as f64;
    for u in ["B", "KB", "MB", "GB", "TB"] {
        if v < 1024.0 || u == "TB" {
            return if u == "B" || u == "KB" { format!("{v:.0} {u}") } else { format!("{v:.2} {u}") };
        }
        v /= 1024.0;
    }
    unreachable!()
}

pub fn fmt_dur(secs: f64) -> String {
    let s = secs.round() as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

fn sort_files(files: &mut [MediaFile]) {
    files.sort_by(|a, b| {
        let ka = (a.created.map(|d| d.timestamp()).unwrap_or(0), a.number.unwrap_or(0), a.chapter.unwrap_or(0));
        let kb = (b.created.map(|d| d.timestamp()).unwrap_or(0), b.number.unwrap_or(0), b.chapter.unwrap_or(0));
        ka.cmp(&kb).then_with(|| a.name.cmp(&b.name))
    });
}

fn num(v: &Value) -> u64 {
    match v {
        Value::Number(n) => n.as_u64().unwrap_or(0),
        Value::String(s) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
}

fn ts(v: &Value) -> Option<DateTime<Local>> {
    let n = num(v);
    if n == 0 {
        return None;
    }
    Local.timestamp_opt(n as i64, 0).single()
}

// ------------------------------------------------------------- discovery
/// Interfaces that look like a GoPro USB link: (ifname, mac-side ip)
pub fn usb_links() -> Vec<(String, String)> {
    static RIF: OnceLock<Regex> = OnceLock::new();
    static RIP: OnceLock<Regex> = OnceLock::new();
    let out = match Command::new("ifconfig").output() {
        Ok(o) => String::from_utf8_lossy(&o.stdout).to_string(),
        Err(_) => return vec![],
    };
    let mut cur = String::new();
    let mut found = vec![];
    for line in out.lines() {
        if let Some(c) = re(r"^([A-Za-z0-9]+):", &RIF).captures(line) {
            cur = c[1].to_string();
        }
        if let Some(c) = re(r"\binet (172\.2\d\.1\d\d\.\d+)\b", &RIP).captures(line) {
            found.push((cur.clone(), c[1].to_string()));
        }
    }
    found
}

/// (camera ip, how we decided)
pub fn discover(explicit: Option<&str>) -> (String, String) {
    if let Some(ip) = explicit {
        return (ip.to_string(), "given with --ip".into());
    }
    if let Some((ifname, ip)) = usb_links().into_iter().next() {
        let mut o: Vec<&str> = ip.split('.').collect();
        o[3] = "51";
        return (o.join("."), format!("USB link {ifname}, Mac is {ip}"));
    }
    (WIFI_IP.into(), "no USB link found; trying GoPro Wi-Fi address".into())
}

pub fn find_sd_cards() -> Vec<PathBuf> {
    let mut v = vec![];
    if let Ok(rd) = fs::read_dir("/Volumes") {
        for e in rd.flatten() {
            let dcim = e.path().join("DCIM");
            if let Ok(sub) = fs::read_dir(&dcim) {
                if sub.flatten().any(|d| d.file_name().to_string_lossy().to_uppercase().ends_with("GOPRO")) {
                    v.push(dcim);
                }
            }
        }
    }
    v
}

// ---------------------------------------------------------------- camera
#[derive(Clone)]
pub struct Camera {
    pub ip: String,
    pub port: u16,
    base: String,
    client: reqwest::blocking::Client,
}

pub struct KeepAlive {
    stop: Arc<AtomicBool>,
}
impl Drop for KeepAlive {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

#[derive(Clone, Debug, Default)]
pub struct CamState {
    pub battery: Option<u8>,
    pub free: Option<u64>,
    pub total: Option<u64>,
    pub recording: bool,
    pub busy: bool,
}

impl Camera {
    pub fn new(ip: &str, port: u16) -> Self {
        let client = reqwest::blocking::Client::builder()
            .timeout(None)
            .connect_timeout(Duration::from_secs(5))
            .user_agent("gopro-import/0.1")
            .build()
            .expect("http client");
        Camera { ip: ip.into(), port, base: format!("http://{ip}:{port}"), client }
    }

    fn url(&self, path: &str, params: &[(&str, &str)]) -> String {
        // The camera does not decode %2F, so keep '/' literal and only escape what must be.
        fn enc(v: &str) -> String {
            let mut o = String::new();
            for b in v.bytes() {
                match b {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => o.push(b as char),
                    _ => o.push_str(&format!("%{b:02X}")),
                }
            }
            o
        }
        let mut u = format!("{}{}", self.base, path);
        for (i, (k, v)) in params.iter().enumerate() {
            u.push(if i == 0 { '?' } else { '&' });
            u.push_str(&format!("{}={}", enc(k), enc(v)));
        }
        u
    }

    pub fn get_bytes(&self, path: &str, params: &[(&str, &str)], timeout: Duration) -> Result<Vec<u8>> {
        let resp = self.client.get(self.url(path, params)).timeout(timeout).send()?;
        if !resp.status().is_success() {
            bail!("HTTP {} for {}", resp.status(), path);
        }
        Ok(resp.bytes()?.to_vec())
    }

    pub fn get_json(&self, path: &str, params: &[(&str, &str)], timeout: Duration) -> Result<Value> {
        let b = self.get_bytes(path, params, timeout)?;
        if b.iter().all(|c| c.is_ascii_whitespace()) {
            return Ok(Value::Object(Default::default()));
        }
        Ok(serde_json::from_slice(&b).unwrap_or(Value::Null))
    }

    pub fn reachable(&self) -> bool {
        // generous: a running multi-stream import saturates the camera's little HTTP server
        self.get_json("/gopro/camera/info", &[], Duration::from_secs(10)).is_ok()
    }

    pub fn wired_control(&self, on: bool) {
        let _ = self.get_json("/gopro/camera/control/wired_usb", &[("p", if on { "1" } else { "0" })], Duration::from_secs(5));
    }

    pub fn info(&self) -> Value {
        match self.get_json("/gopro/camera/info", &[], Duration::from_secs(5)) {
            Ok(v) => v.get("info").cloned().unwrap_or(v),
            Err(_) => Value::Null,
        }
    }

    pub fn state(&self) -> Result<CamState> {
        let v = self.get_json("/gopro/camera/state", &[], Duration::from_secs(8))?;
        let st = v.get("status").cloned().unwrap_or(Value::Null);
        let g = |k: &str| st.get(k).map(num);
        Ok(CamState {
            battery: g("70").map(|b| b as u8),
            free: g("54").map(|kb| kb * 1024),
            total: None,
            recording: g("10").unwrap_or(0) == 1,
            busy: g("8").unwrap_or(0) == 1,
        })
    }

    pub fn keepalive(&self) -> KeepAlive {
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = stop.clone();
        let cam = self.clone();
        thread::spawn(move || {
            while !s2.load(Ordering::Relaxed) {
                let _ = cam.get_bytes("/gopro/camera/keep_alive", &[], Duration::from_secs(3));
                for _ in 0..30 {
                    if s2.load(Ordering::Relaxed) {
                        return;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
            }
        });
        KeepAlive { stop }
    }

    pub fn turbo(&self, on: bool) {
        let _ = self.get_json("/gopro/media/turbo_transfer", &[("p", if on { "1" } else { "0" })], Duration::from_secs(5));
    }

    pub fn media_list(&self) -> Result<Vec<MediaFile>> {
        let v = self.get_json("/gopro/media/list", &[], Duration::from_secs(30)).context("media list")?;
        let mut files = vec![];
        for d in v.get("media").and_then(|m| m.as_array()).cloned().unwrap_or_default() {
            let folder = d.get("d").and_then(|x| x.as_str()).unwrap_or("100GOPRO").to_string();
            for f in d.get("fs").and_then(|m| m.as_array()).cloned().unwrap_or_default() {
                let name = f.get("n").and_then(|x| x.as_str()).unwrap_or("").to_string();
                if name.is_empty() {
                    continue;
                }
                let created = f.get("cre").and_then(ts);
                let lrv = f.get("glrv").map(num).unwrap_or(0);
                let gtype = f.get("t").and_then(|x| x.as_str()).map(|s| s.to_string());
                let (kind, chapter, number, ext) = parse_name(&name);
                let grouped = f.get("g").is_some() && f.get("b").is_some() && f.get("l").is_some() && kind == Kind::Photo;
                if grouped {
                    let g = num(&f["g"]);
                    let (first, last) = (num(&f["b"]), num(&f["l"]));
                    let missing: Vec<u64> = f.get("m").and_then(|m| m.as_array()).map(|a| a.iter().map(num).collect()).unwrap_or_default();
                    let count = (last - first + 1).saturating_sub(missing.len() as u64).max(1);
                    let per = num(&f["s"]) / count;
                    for seq in first..=last {
                        if missing.contains(&seq) {
                            continue;
                        }
                        let n = format!("G{g:03}{seq:04}{ext}");
                        let (k, c, nn, e) = parse_name(&n);
                        files.push(MediaFile {
                            path: format!("{folder}/{n}"),
                            folder: folder.clone(),
                            name: n,
                            size: per,
                            size_estimated: true,
                            created,
                            kind: k,
                            chapter: c,
                            number: nn,
                            group_type: gtype.clone(),
                            ext: e,
                            lrv_size: 0,
                            local: None,
                        });
                    }
                } else {
                    files.push(MediaFile {
                        path: format!("{folder}/{name}"),
                        folder: folder.clone(),
                        name,
                        size: num(&f["s"]),
                        size_estimated: false,
                        created,
                        kind,
                        chapter,
                        number,
                        group_type: gtype,
                        ext,
                        lrv_size: lrv,
                        local: None,
                    });
                }
            }
        }
        sort_files(&mut files);
        Ok(files)
    }

    pub fn media_info(&self, path: &str) -> Value {
        self.get_json("/gopro/media/info", &[("path", path)], Duration::from_secs(15)).unwrap_or(Value::Null)
    }

    pub fn thumbnail(&self, path: &str, big: bool) -> Result<Vec<u8>> {
        let ep = if big { "/gopro/media/screennail" } else { "/gopro/media/thumbnail" };
        self.get_bytes(ep, &[("path", path)], Duration::from_secs(20))
    }

    pub fn delete(&self, path: &str) -> Result<()> {
        self.get_json("/gopro/media/delete/file", &[("path", path)], Duration::from_secs(15))?;
        Ok(())
    }

    pub fn media_url(&self, path: &str) -> String {
        format!("{}/videos/DCIM/{}", self.base, path)
    }

    /// Multi-connection resumable download: the file is split into `streams` ranges fetched
    /// concurrently and written in place; per-segment progress is kept in `<dest>.part.json`.
    pub fn download_parallel(
        &self,
        path: &str,
        dest: &Path,
        expected: u64,
        streams: usize,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(u64, Option<u64>, u64),
    ) -> Result<u64> {
        use std::os::unix::fs::FileExt;
        use std::sync::atomic::AtomicU64;
        let tmp = PathBuf::from(format!("{}.part", dest.display()));
        let state_path = PathBuf::from(format!("{}.part.json", dest.display()));
        let have = fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
        let mut segs: Vec<(u64, u64, u64)> = vec![];
        if let Ok(txt) = fs::read_to_string(&state_path) {
            if let Ok(v) = serde_json::from_str::<Value>(&txt) {
                if v.get("size").map(num) == Some(expected) && have == expected {
                    segs = v
                        .get("segments")
                        .and_then(|a| a.as_array())
                        .map(|a| a.iter().filter_map(|x| x.as_array().map(|t| (num(&t[0]), num(&t[1]), num(&t[2])))).collect())
                        .unwrap_or_default();
                }
            }
        }
        if segs.is_empty() {
            let n = streams.max(1) as u64;
            let mut start = 0u64;
            if have > 0 && have < expected && !state_path.exists() {
                segs.push((0, have, have)); // keep a legacy single-stream partial
                start = have;
            }
            let seg_len = ((expected - start) + n - 1) / n;
            let mut s0 = start;
            while s0 < expected {
                let e = (s0 + seg_len.max(1)).min(expected);
                segs.push((s0, e, 0));
                s0 = e;
            }
        }
        let file = OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&tmp)?;
        if file.metadata()?.len() != expected {
            file.set_len(expected)?;
        }
        let dones: Vec<AtomicU64> = segs.iter().map(|s| AtomicU64::new(s.2)).collect();
        let base_done: u64 = segs.iter().map(|s| s.2).sum();
        let errors: Mutex<Vec<String>> = Mutex::new(vec![]);
        let abort = AtomicBool::new(false);
        let url = self.media_url(path);
        let save_state = |dones: &Vec<AtomicU64>| {
            let v = serde_json::json!({
                "size": expected,
                "segments": segs.iter().zip(dones).map(|(s, d)| vec![s.0, s.1, d.load(Ordering::Relaxed).min(s.1 - s.0)]).collect::<Vec<_>>(),
            });
            let _ = fs::write(&state_path, v.to_string());
        };
        thread::scope(|sc| {
            for (i, seg) in segs.iter().enumerate() {
                let (start, end, _) = *seg;
                let (done, file, client, url, errors, abort) = (&dones[i], &file, &self.client, &url, &errors, &abort);
                if done.load(Ordering::Relaxed) >= end - start {
                    continue;
                }
                sc.spawn(move || {
                    let mut attempt = 0u64;
                    loop {
                        if cancel.load(Ordering::Relaxed) || abort.load(Ordering::Relaxed) {
                            return;
                        }
                        let cur = start + done.load(Ordering::Relaxed);
                        if cur >= end {
                            return;
                        }
                        let r = (|| -> Result<()> {
                            let mut resp = client.get(url).header(reqwest::header::RANGE, format!("bytes={}-{}", cur, end - 1)).send()?;
                            if resp.status().as_u16() != 206 {
                                bail!("HTTP {} (range not honoured)", resp.status());
                            }
                            let mut buf = vec![0u8; CHUNK];
                            let mut off = cur;
                            loop {
                                if cancel.load(Ordering::Relaxed) || abort.load(Ordering::Relaxed) {
                                    bail!("cancelled");
                                }
                                let n = resp.read(&mut buf)?;
                                if n == 0 {
                                    break;
                                }
                                let n = (n as u64).min(end - off) as usize;
                                file.write_at(&buf[..n], off)?;
                                off += n as u64;
                                done.fetch_add(n as u64, Ordering::Relaxed);
                                if off >= end {
                                    break;
                                }
                            }
                            Ok(())
                        })();
                        match r {
                            Ok(()) => {}
                            Err(e) => {
                                if e.to_string().contains("cancelled") {
                                    return;
                                }
                                attempt += 1;
                                if attempt > 4 {
                                    errors.lock().unwrap().push(format!("segment {i}: {e:#}"));
                                    abort.store(true, Ordering::Relaxed);
                                    return;
                                }
                                thread::sleep(Duration::from_millis(400 * attempt));
                            }
                        }
                    }
                });
            }
            let mut last_save = Instant::now();
            loop {
                let total: u64 = dones.iter().map(|d| d.load(Ordering::Relaxed)).sum::<u64>().min(expected);
                progress(total, Some(expected), total - base_done);
                if last_save.elapsed() > Duration::from_secs(1) {
                    save_state(&dones);
                    last_save = Instant::now();
                }
                if total >= expected || cancel.load(Ordering::Relaxed) || abort.load(Ordering::Relaxed) {
                    break;
                }
                thread::sleep(Duration::from_millis(120));
            }
        });
        let total: u64 = dones.iter().map(|d| d.load(Ordering::Relaxed)).sum::<u64>().min(expected);
        save_state(&dones);
        if cancel.load(Ordering::Relaxed) && total < expected {
            bail!("cancelled");
        }
        let errs = errors.into_inner().unwrap();
        if !errs.is_empty() {
            bail!("{}", errs.join("; "));
        }
        if total != expected {
            bail!("incomplete: {total} of {expected} bytes");
        }
        drop(file);
        let _ = fs::remove_file(&state_path);
        fs::rename(&tmp, dest)?;
        Ok(expected)
    }

    /// Resumable download. progress(done_bytes, total, bytes_moved_this_session)
    pub fn download(
        &self,
        path: &str,
        dest: &Path,
        expected: Option<u64>,
        lenient: bool,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(u64, Option<u64>, u64),
    ) -> Result<u64> {
        let tmp = PathBuf::from(format!("{}.part", dest.display()));
        let have = fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0);
        let mut req = self.client.get(format!("{}/videos/DCIM/{}", self.base, path));
        if have > 0 {
            req = req.header(reqwest::header::RANGE, format!("bytes={have}-"));
        }
        let mut resp = req.send().context("connect to camera")?;
        let code = resp.status().as_u16();
        if code == 416 && have > 0 && expected.map_or(true, |e| e == have) {
            fs::rename(&tmp, dest)?;
            return Ok(have);
        }
        if !resp.status().is_success() {
            bail!("HTTP {}", resp.status());
        }
        let start = if have > 0 && code == 206 { have } else { 0 };
        let total = expected.or_else(|| resp.content_length().map(|cl| start + cl));
        let mut out = if start > 0 {
            OpenOptions::new().append(true).open(&tmp)?
        } else {
            File::create(&tmp)?
        };
        let mut buf = vec![0u8; CHUNK];
        let mut done = start;
        loop {
            if cancel.load(Ordering::Relaxed) {
                out.flush()?;
                bail!("cancelled");
            }
            let n = resp.read(&mut buf).context("read from camera")?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])?;
            done += n as u64;
            progress(done, total, done - start);
        }
        out.flush()?;
        drop(out);
        if let Some(e) = expected {
            if !lenient && done != e {
                bail!("size mismatch: got {done}, camera says {e}");
            }
        }
        fs::rename(&tmp, dest)?;
        Ok(done)
    }
}

// --------------------------------------------------------------- SD card
pub fn sd_media_list(dcim: &Path) -> Result<Vec<MediaFile>> {
    let mut files = vec![];
    let mut folders: Vec<PathBuf> = fs::read_dir(dcim)?.flatten().map(|e| e.path()).filter(|p| p.is_dir()).collect();
    folders.sort();
    for fp in folders {
        let folder = fp.file_name().unwrap().to_string_lossy().to_string();
        for e in fs::read_dir(&fp)?.flatten() {
            let p = e.path();
            if !p.is_file() {
                continue;
            }
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            let (kind, chapter, number, ext) = parse_name(&name);
            if !matches!(ext.as_str(), ".MP4" | ".JPG" | ".GPR" | ".360" | ".WAV" | ".RAW" | ".HEIC" | ".LRV" | ".THM") {
                continue;
            }
            let md = e.metadata()?;
            let created = md.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).and_then(|d| Local.timestamp_opt(d.as_secs() as i64, 0).single());
            files.push(MediaFile {
                path: format!("{folder}/{name}"),
                folder: folder.clone(),
                name,
                size: md.len(),
                size_estimated: false,
                created,
                kind,
                chapter,
                number,
                group_type: None,
                ext,
                lrv_size: 0,
                local: Some(p),
            });
        }
    }
    sort_files(&mut files);
    Ok(files)
}

// ---------------------------------------------------------------- source
pub enum Source {
    Camera { cam: Camera, info: Value },
    Sd(PathBuf),
}

impl Source {
    pub fn connect(ip: Option<&str>, port: u16, sd: Option<&Path>) -> Result<Source> {
        if let Some(sd) = sd {
            let dcim = if sd.file_name().map(|n| n.to_string_lossy().to_uppercase()) == Some("DCIM".into()) {
                sd.to_path_buf()
            } else {
                sd.join("DCIM")
            };
            if !dcim.is_dir() {
                bail!("no DCIM folder under {}", sd.display());
            }
            return Ok(Source::Sd(dcim));
        }
        let (addr, how) = discover(ip);
        let cam = Camera::new(&addr, port);
        if !cam.reachable() {
            let mut msg = format!(
                "camera at {addr}:{port} is not answering ({how}).\n  \
                 Wake it (press Mode) — the screen should say USB Connected — then retry.\n  \
                 Camera: Preferences > Connections > USB Connection = GoPro Connect (not MTP)."
            );
            if let Some(c) = find_sd_cards().first() {
                msg.push_str(&format!("\n  An SD card is mounted; try:  gopro --sd {}", c.display()));
            }
            bail!(msg);
        }
        cam.wired_control(true);
        let info = cam.info();
        Ok(Source::Camera { cam, info })
    }

    pub fn is_camera(&self) -> bool {
        matches!(self, Source::Camera { .. })
    }

    pub fn camera(&self) -> Option<&Camera> {
        match self {
            Source::Camera { cam, .. } => Some(cam),
            _ => None,
        }
    }

    pub fn model(&self) -> String {
        match self {
            Source::Camera { info, .. } => info.get("model_name").and_then(|v| v.as_str()).unwrap_or("GoPro").to_string(),
            Source::Sd(_) => "SD card".into(),
        }
    }

    pub fn label(&self) -> String {
        match self {
            Source::Camera { cam, info } => format!(
                "{} · fw {} · {}",
                info.get("model_name").and_then(|v| v.as_str()).unwrap_or("GoPro"),
                info.get("firmware_version").and_then(|v| v.as_str()).unwrap_or("?"),
                cam.ip
            ),
            Source::Sd(p) => format!("SD card {}", p.display()),
        }
    }

    pub fn list(&self) -> Result<Vec<MediaFile>> {
        match self {
            Source::Camera { cam, .. } => cam.media_list(),
            Source::Sd(p) => sd_media_list(p),
        }
    }

    pub fn state(&self) -> CamState {
        match self {
            Source::Camera { cam, .. } => cam.state().unwrap_or_default(),
            Source::Sd(p) => {
                let mut st = CamState::default();
                if let Ok(out) = Command::new("df").arg("-k").arg(p).output() {
                    let s = String::from_utf8_lossy(&out.stdout);
                    if let Some(line) = s.lines().nth(1) {
                        let cols: Vec<&str> = line.split_whitespace().collect();
                        if cols.len() > 3 {
                            st.total = cols[1].parse::<u64>().ok().map(|k| k * 1024);
                            st.free = cols[3].parse::<u64>().ok().map(|k| k * 1024);
                        }
                    }
                }
                st
            }
        }
    }

    pub fn meta(&self, f: &MediaFile) -> Value {
        match self {
            Source::Camera { cam, .. } => cam.media_info(&f.path),
            Source::Sd(_) => Value::Null,
        }
    }

    fn cache_dir() -> PathBuf {
        let d = dirs::cache_dir().unwrap_or_else(|| PathBuf::from("/tmp")).join("gopro-import/thumbs");
        let _ = fs::create_dir_all(&d);
        d
    }

    /// Thumbnail as a decoded image (disk-cached). None if unavailable.
    pub fn thumb(&self, f: &MediaFile, big: bool) -> Option<DynamicImage> {
        let key = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            (&f.path, f.size, big).hash(&mut h);
            format!("{:016x}.jpg", h.finish())
        };
        let cached = Self::cache_dir().join(key);
        if let Ok(b) = fs::read(&cached) {
            if let Ok(img) = image::load_from_memory(&b) {
                return Some(img);
            }
        }
        let bytes: Vec<u8> = match self {
            Source::Camera { cam, .. } => cam.thumbnail(&f.path, big).ok()?,
            Source::Sd(_) => {
                let local = f.local.as_ref()?;
                if f.kind == Kind::Photo {
                    let img = image::open(local).ok()?;
                    let (w, h) = if big { (960, 720) } else { (320, 240) };
                    let t = img.thumbnail(w, h);
                    let mut buf = std::io::Cursor::new(Vec::new());
                    t.to_rgb8().write_to(&mut buf, image::ImageFormat::Jpeg).ok()?;
                    buf.into_inner()
                } else {
                    let thm = local.with_extension("THM");
                    fs::read(thm).ok()?
                }
            }
        };
        let img = image::load_from_memory(&bytes).ok()?;
        let _ = fs::write(&cached, &bytes);
        Some(img)
    }

    /// Copy/download one file into `dest`. Returns bytes.
    pub fn fetch(
        &self,
        f: &MediaFile,
        dest: &Path,
        streams: usize,
        cancel: &AtomicBool,
        progress: &mut dyn FnMut(u64, Option<u64>, u64),
    ) -> Result<u64> {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let n = match self {
            Source::Camera { cam, .. } => {
                if !f.size_estimated && streams > 1 && f.size >= 8 << 20 {
                    cam.download_parallel(&f.path, dest, f.size, streams, cancel, progress)?
                } else {
                    let expected = if f.size_estimated { None } else { Some(f.size) };
                    cam.download(&f.path, dest, expected, f.size_estimated, cancel, progress)?
                }
            }
            Source::Sd(_) => {
                let local = f.local.as_ref().ok_or_else(|| anyhow!("no local path"))?;
                let tmp = PathBuf::from(format!("{}.part", dest.display()));
                let mut inp = File::open(local)?;
                let mut out = File::create(&tmp)?;
                let mut buf = vec![0u8; CHUNK];
                let mut done = 0u64;
                loop {
                    if cancel.load(Ordering::Relaxed) {
                        bail!("cancelled");
                    }
                    let n = inp.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    out.write_all(&buf[..n])?;
                    done += n as u64;
                    progress(done, Some(f.size), done);
                }
                drop(out);
                fs::rename(&tmp, dest)?;
                done
            }
        };
        if let Some(c) = f.created {
            let t = SystemTime::UNIX_EPOCH + Duration::from_secs(c.timestamp().max(0) as u64);
            if let Ok(fh) = File::options().write(true).open(dest) {
                let _ = fh.set_times(fs::FileTimes::new().set_modified(t));
            }
        }
        Ok(n)
    }

    pub fn delete(&self, f: &MediaFile) -> Result<()> {
        match self {
            Source::Camera { cam, .. } => cam.delete(&f.path),
            Source::Sd(_) => Ok(fs::remove_file(f.local.as_ref().ok_or_else(|| anyhow!("no local path"))?)?),
        }
    }
}

/// Destination path for a file: `<dest>/<YYYY-MM-DD>/<name>` (or flat).
pub fn target_path(dest: &Path, f: &MediaFile, flat: bool) -> PathBuf {
    if flat {
        dest.join(&f.name)
    } else {
        dest.join(f.day()).join(&f.name)
    }
}

/// Already imported with matching size?
pub fn already_have(target: &Path, f: &MediaFile) -> bool {
    match fs::metadata(target) {
        Ok(m) => f.size_estimated || m.len() == f.size,
        Err(_) => false,
    }
}
