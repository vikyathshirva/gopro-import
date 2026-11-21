//! Plain command-line subcommands (find / status / ls / pull / rm / open).
use crate::camera::{already_have, discover, find_sd_cards, human, target_path, usb_links, Camera, Kind, MediaFile, Source};
use anyhow::{bail, Result};
use chrono::{Local, NaiveDate, TimeZone};
use indicatif::{ProgressBar, ProgressStyle};
use std::{
    io::{self, Write},
    path::Path,
    sync::atomic::AtomicBool,
    time::Instant,
};

pub struct Filter<'a> {
    pub since: Option<&'a str>,
    pub names: &'a [String],
    pub include_lrv: bool,
}

pub fn apply_filter(files: Vec<MediaFile>, f: &Filter) -> Result<Vec<MediaFile>> {
    let since = match f.since {
        Some(s) => {
            let d = NaiveDate::parse_from_str(s, "%Y-%m-%d").map_err(|_| anyhow::anyhow!("--since must be YYYY-MM-DD"))?;
            Some(Local.from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap()).single().unwrap())
        }
        None => None,
    };
    Ok(files
        .into_iter()
        .filter(|m| f.include_lrv || !m.is_sidecar())
        .filter(|m| since.map_or(true, |s| m.created.map_or(true, |c| c >= s)))
        .filter(|m| m.matches(f.names))
        .collect())
}

pub fn find(ip: Option<&str>, port: u16) {
    for (ifname, mac_ip) in usb_links() {
        let mut o: Vec<&str> = mac_ip.split('.').collect();
        o[3] = "51";
        println!("USB link: {ifname}  Mac={mac_ip}  camera={}:{port}", o.join("."));
    }
    let (addr, how) = discover(ip);
    let cam = Camera::new(&addr, port);
    println!("camera address: {addr}:{port}  ({how})");
    println!("reachable: {}", if cam.reachable() { "yes" } else { "no (asleep, off, or in MTP mode)" });
    for c in find_sd_cards() {
        println!("SD card mounted: {}", c.display());
    }
}

pub fn status(src: &Source) -> Result<()> {
    println!("{}", src.label());
    let st = src.state();
    let mut bits = vec![];
    if let Some(b) = st.battery {
        bits.push(format!("battery {b}%"));
    }
    if let Some(f) = st.free {
        bits.push(format!("free {}", human(f)));
    }
    if st.recording {
        bits.push("RECORDING".into());
    }
    if !bits.is_empty() {
        println!("  {}", bits.join("   "));
    }
    let files = src.list()?;
    let vids: Vec<&MediaFile> = files.iter().filter(|f| f.kind == Kind::Video).collect();
    let pics: Vec<&MediaFile> = files.iter().filter(|f| f.kind == Kind::Photo).collect();
    println!(
        "  {} videos ({}), {} photos ({})",
        vids.len(),
        human(vids.iter().map(|f| f.size).sum()),
        pics.len(),
        human(pics.iter().map(|f| f.size).sum())
    );
    let dates: Vec<_> = files.iter().filter_map(|f| f.created).collect();
    if let (Some(a), Some(b)) = (dates.iter().min(), dates.iter().max()) {
        println!("  from {} to {}", a.format("%Y-%m-%d"), b.format("%Y-%m-%d"));
    }
    Ok(())
}

pub fn ls(src: &Source, filter: &Filter) -> Result<()> {
    let files = apply_filter(src.list()?, filter)?;
    if files.is_empty() {
        println!("no media found");
        return Ok(());
    }
    println!("{:<16}  {:<14}  {:<6}  {:>10}  NOTE", "DATE", "NAME", "TYPE", "SIZE");
    let mut last = String::new();
    for f in &files {
        let day = f.day();
        if !last.is_empty() && day != last {
            println!();
        }
        last = day;
        let when = f.created.map(|d| d.format("%Y-%m-%d %H:%M").to_string()).unwrap_or_else(|| "?".into());
        let mut note = f.note();
        if f.size_estimated {
            note.push_str(" (size approx)");
        }
        println!("{when:<16}  {:<14}  {:<6}  {:>10}  {note}", f.name, f.kind.label(), human(f.size));
    }
    println!("\n{} files, {}", files.len(), human(files.iter().map(|f| f.size).sum()));
    Ok(())
}

pub struct PullOpts<'a> {
    pub dest: &'a Path,
    pub flat: bool,
    pub dry_run: bool,
    pub delete_after: bool,
    pub streams: usize,
}

pub fn pull(src: &Source, filter: &Filter, o: &PullOpts) -> Result<()> {
    let files = apply_filter(src.list()?, filter)?;
    let mut plan = vec![];
    let mut skipped = 0;
    for f in files {
        let t = target_path(o.dest, &f, o.flat);
        if already_have(&t, &f) {
            skipped += 1;
        } else {
            plan.push((f, t));
        }
    }
    let total: u64 = plan.iter().map(|(f, _)| f.size).sum();
    println!("{} files to import ({}), {skipped} already on this Mac -> {}", plan.len(), human(total), o.dest.display());
    if o.dry_run {
        for (f, t) in &plan {
            println!("  would copy {:<26} -> {}", f.path, t.display());
        }
        return Ok(());
    }
    if plan.is_empty() {
        return Ok(());
    }
    let _ka = src.camera().map(|c| {
        c.turbo(true);
        c.keepalive()
    });
    let cancel = AtomicBool::new(false);
    let style = ProgressStyle::with_template("{prefix:<34.bold} {bar:28.cyan/blue} {percent:>3}% {bytes:>10}/{total_bytes:<10} {bytes_per_sec:>10} eta {eta}")
        .unwrap()
        .progress_chars("━╸─");
    let t_all = Instant::now();
    let mut moved = 0u64;
    let mut failed = vec![];
    let n = plan.len();
    for (i, (f, target)) in plan.iter().enumerate() {
        let pb = ProgressBar::new(f.size.max(1));
        pb.set_style(style.clone());
        pb.set_prefix(format!("[{}/{n}] {} {}", i + 1, f.icon(), f.name));
        let r = src.fetch(f, target, o.streams, &cancel, &mut |done, total, _| {
            if let Some(t) = total {
                pb.set_length(t.max(1));
            }
            pb.set_position(done);
        });
        match r {
            Ok(bytes) => {
                pb.finish();
                moved += bytes;
                if o.delete_after {
                    match src.delete(f) {
                        Ok(_) => println!("      deleted from camera: {}", f.name),
                        Err(e) => println!("      could not delete {}: {e}", f.name),
                    }
                }
            }
            Err(e) => {
                pb.abandon_with_message(format!("FAILED: {e}"));
                failed.push(f.name.clone());
            }
        }
    }
    if let Some(c) = src.camera() {
        c.turbo(false);
    }
    let secs = t_all.elapsed().as_secs_f64().max(0.001);
    println!(
        "\nimported {} files, {} in {:.0}s ({}/s) -> {}",
        n - failed.len(),
        human(moved),
        secs,
        human((moved as f64 / secs) as u64),
        o.dest.display()
    );
    if !failed.is_empty() {
        bail!("failed: {} (rerun `gopro pull` to retry/resume)", failed.join(", "));
    }
    Ok(())
}

pub fn play(src: &Source, name: &str, full: bool) -> Result<()> {
    let files = apply_filter(src.list()?, &Filter { since: None, names: &[name.to_string()], include_lrv: false })?;
    let f = files.iter().find(|f| f.kind == Kind::Video).ok_or_else(|| anyhow::anyhow!("no video named {name} on the camera"))?;
    let target = crate::player::playback_source(src, f, !full);
    let app = crate::player::open_external(&target)?;
    println!("playing {} in {app}{}\n  {target}", f.name, if target.ends_with(".LRV") { " (low-res proxy; --full for the original)" } else { "" });
    Ok(())
}

pub fn rm(src: &Source, names: &[String], yes: bool) -> Result<()> {
    let files = apply_filter(src.list()?, &Filter { since: None, names, include_lrv: true })?;
    if files.is_empty() {
        bail!("no matching files on the camera");
    }
    for f in &files {
        println!("  {}  {}", f.path, human(f.size));
    }
    if !yes {
        print!("delete {} file(s) from the camera? [y/N] ", files.len());
        io::stdout().flush()?;
        let mut s = String::new();
        io::stdin().read_line(&mut s)?;
        if s.trim().to_lowercase() != "y" {
            println!("cancelled");
            return Ok(());
        }
    }
    for f in &files {
        src.delete(f)?;
        println!("deleted {}", f.path);
    }
    Ok(())
}
