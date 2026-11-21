#!/usr/bin/env python3
"""
gopro-py — Python fallback CLI for reading/importing GoPro media (HERO9+ / HERO12) on a Mac.
Primary tool is the Rust `gopro` binary; this remains as a stdlib-only fallback.

  gopro-py find | status | ls [--since D] | pull [NAMES] [--dest DIR --since D --dry-run --delete-after --flat --include-lrv] | rm NAMES | open
  --ip ADDR --port N --sd /Volumes/CARD
"""
import argparse, datetime as dt, json, os, re, shutil, subprocess, sys, threading, time
import urllib.error, urllib.parse, urllib.request

DEFAULT_DEST = os.path.expanduser("~/Movies/GoPro")
WIFI_IP = "10.5.5.9"
CHUNK = 1 << 20
MEDIA_EXT = {".MP4", ".JPG", ".GPR", ".360", ".WAV", ".RAW", ".HEIC"}
SIDE_EXT = {".LRV", ".THM"}


def human(n):
    n = float(n)
    for unit in ("B", "KB", "MB", "GB", "TB"):
        if n < 1024 or unit == "TB":
            return f"{n:,.0f} {unit}" if unit in ("B", "KB") else f"{n:,.2f} {unit}"
        n /= 1024


def ts_to_dt(ts):
    try:
        return dt.datetime.fromtimestamp(int(ts))
    except (TypeError, ValueError, OSError):
        return None


def parse_name(name):
    stem, ext = os.path.splitext(name); ext = ext.upper()
    info = {"kind": "other", "chapter": None, "number": None, "ext": ext}
    m = re.fullmatch(r"(GX|GH|GL|GP|GS)(\d{2})(\d{4})", stem, re.I)
    if m:
        info.update(kind="video", chapter=int(m.group(2)), number=int(m.group(3))); return info
    m = re.fullmatch(r"GOPR(\d{4})", stem, re.I)
    if m:
        info.update(kind="video" if ext == ".MP4" else "photo", chapter=1, number=int(m.group(1))); return info
    m = re.fullmatch(r"G(\d{3})(\d{4})", stem, re.I)
    if m:
        info.update(kind="photo", chapter=int(m.group(1)), number=int(m.group(2))); return info
    if ext in (".JPG", ".GPR", ".HEIC"): info["kind"] = "photo"
    elif ext in (".MP4", ".360"): info["kind"] = "video"
    return info


def die(msg, code=1):
    print(f"error: {msg}", file=sys.stderr); sys.exit(code)


def usb_interfaces():
    out = subprocess.run(["ifconfig"], capture_output=True, text=True).stdout
    found, cur = [], None
    for line in out.splitlines():
        if line and not line[0].isspace(): cur = line.split(":")[0]
        m = re.search(r"\binet (172\.2\d\.1\d\d\.\d+)\b", line)
        if m and cur: found.append((cur, m.group(1)))
    return found


def discover(explicit_ip=None):
    if explicit_ip: return explicit_ip, "given with --ip"
    for ifname, ip in usb_interfaces():
        return ".".join(ip.split(".")[:3] + ["51"]), f"USB link {ifname}, Mac is {ip}"
    return WIFI_IP, "no USB link found; trying GoPro Wi-Fi address"


def find_sd_cards():
    cards = []
    if os.path.isdir("/Volumes"):
        for v in os.listdir("/Volumes"):
            dcim = os.path.join("/Volumes", v, "DCIM")
            if os.path.isdir(dcim) and any(d.upper().endswith("GOPRO") for d in os.listdir(dcim)): cards.append(dcim)
    return cards


class Camera:
    def __init__(self, ip, port=8080, timeout=8):
        self.base = f"http://{ip}:{port}"; self.ip = ip; self.timeout = timeout; self._keep = None; self.lenient = False

    def get(self, path, params=None, timeout=None, raw=False):
        url = self.base + path + ("?" + urllib.parse.urlencode(params) if params else "")
        with urllib.request.urlopen(urllib.request.Request(url), timeout=timeout or self.timeout) as r: data = r.read()
        if raw: return data
        try: return json.loads(data) if data.strip() else {}
        except json.JSONDecodeError: return {"_raw": data.decode(errors="replace")}

    def reachable(self):
        try: self.get("/gopro/camera/info", timeout=3); return True
        except Exception: return False

    def wired_control(self, on=True):
        try: self.get("/gopro/camera/control/wired_usb", {"p": 1 if on else 0})
        except Exception: pass

    def info(self):
        d = self.get("/gopro/camera/info"); return d.get("info", d)

    def state(self): return self.get("/gopro/camera/state")

    def keep_alive(self):
        try: self.get("/gopro/camera/keep_alive", timeout=3)
        except Exception: pass

    def start_keepalive(self):
        stop = threading.Event()
        def loop():
            while not stop.wait(3.0): self.keep_alive()
        threading.Thread(target=loop, daemon=True).start(); self._keep = stop

    def stop_keepalive(self):
        if self._keep: self._keep.set()

    def turbo(self, on):
        try: self.get("/gopro/media/turbo_transfer", {"p": 1 if on else 0})
        except Exception: pass

    def media_list(self):
        data = self.get("/gopro/media/list", timeout=30); files = []
        for d in data.get("media", []):
            folder = d.get("d", "100GOPRO")
            for f in d.get("fs", []):
                name = f["n"]
                base = {"folder": folder, "created": ts_to_dt(f.get("cre")), "modified": ts_to_dt(f.get("mod")),
                        "lrv_size": int(f.get("glrv", 0) or 0), "group": f.get("g"), "group_type": f.get("t")}
                if f.get("g") and f.get("b") is not None and f.get("l") is not None and parse_name(name)["kind"] == "photo":
                    g = int(f["g"]); ext = os.path.splitext(name)[1]; missing = {int(x) for x in f.get("m", [])}
                    first, last = int(f["b"]), int(f["l"])
                    per = int(f.get("s", 0) or 0) // max(1, (last - first + 1 - len(missing)))
                    for seq in range(first, last + 1):
                        if seq in missing: continue
                        files.append({**base, "name": f"G{g:03d}{seq:04d}{ext}", "size": per, "size_estimated": True})
                else:
                    files.append({**base, "name": name, "size": int(f.get("s", 0) or 0), "size_estimated": False})
        for rec in files:
            rec.update(parse_name(rec["name"])); rec["path"] = f"{rec['folder']}/{rec['name']}"
        files.sort(key=lambda r: (r["created"] or dt.datetime.min, r["number"] or 0, r["chapter"] or 0, r["name"]))
        return files

    def media_url(self, path): return f"{self.base}/videos/DCIM/{path}"

    def delete(self, path): self.get("/gopro/media/delete/file", {"path": path})

    def download(self, path, dest, expected_size=None, progress=None):
        tmp = dest + ".part"; have = os.path.getsize(tmp) if os.path.exists(tmp) else 0
        headers = {"Range": f"bytes={have}-"} if have else {}
        req = urllib.request.Request(self.media_url(path), headers=headers)
        try: resp = urllib.request.urlopen(req, timeout=30)
        except urllib.error.HTTPError as e:
            if e.code == 416 and have and (expected_size is None or have == expected_size):
                os.replace(tmp, dest); return have
            raise
        mode = "ab"
        if have and resp.status != 206: have, mode = 0, "wb"
        total = expected_size
        if total is None and resp.headers.get("Content-Length"): total = have + int(resp.headers["Content-Length"])
        done = start = have; t0 = time.time()
        with open(tmp, mode) as out, resp:
            while True:
                chunk = resp.read(CHUNK)
                if not chunk: break
                out.write(chunk); done += len(chunk)
                if progress: progress(done, total, time.time() - t0, done - start)
        if expected_size is not None and not self.lenient and done != expected_size:
            raise IOError(f"size mismatch: got {done}, camera says {expected_size}")
        os.replace(tmp, dest); return done


def sd_media_list(dcim):
    files = []
    for folder in sorted(os.listdir(dcim)):
        fp = os.path.join(dcim, folder)
        if not os.path.isdir(fp): continue
        for name in sorted(os.listdir(fp)):
            full = os.path.join(fp, name); ext = os.path.splitext(name)[1].upper()
            if not os.path.isfile(full) or ext not in MEDIA_EXT | SIDE_EXT: continue
            st = os.stat(full)
            rec = {"folder": folder, "name": name, "size": st.st_size, "size_estimated": False,
                   "created": dt.datetime.fromtimestamp(st.st_mtime), "modified": None, "lrv_size": 0,
                   "group": None, "group_type": None, "local": full}
            rec.update(parse_name(name)); rec["path"] = f"{folder}/{name}"; files.append(rec)
    files.sort(key=lambda r: (r["created"] or dt.datetime.min, r["number"] or 0, r["chapter"] or 0, r["name"]))
    return files


def connect(args):
    if args.sd:
        dcim = args.sd if os.path.basename(args.sd).upper() == "DCIM" else os.path.join(args.sd, "DCIM")
        if not os.path.isdir(dcim): die(f"no DCIM folder at {args.sd}")
        return None, dcim
    ip, how = discover(args.ip); cam = Camera(ip, args.port)
    if not cam.reachable():
        cards = find_sd_cards(); hint = f"\n  SD card mounted; try --sd {cards[0]}" if cards else ""
        die(f"camera at {ip}:{args.port} not answering ({how}).\n  Wake it (Mode button), screen should say USB Connected;"
            f" USB Connection must be 'GoPro Connect'.{hint}")
    cam.wired_control(True); return cam, None


def filter_files(files, args, names=None):
    out = []; since = dt.datetime.strptime(args.since, "%Y-%m-%d") if getattr(args, "since", None) else None
    wanted = {n.upper() for n in (names or [])}
    for f in files:
        if f["ext"] in SIDE_EXT and not getattr(args, "include_lrv", False): continue
        if since and f["created"] and f["created"] < since: continue
        if wanted and f["name"].upper() not in wanted and os.path.splitext(f["name"])[0].upper() not in wanted: continue
        out.append(f)
    return out


def cmd_find(args):
    for ifname, ip in usb_interfaces(): print(f"USB link: {ifname}  Mac={ip}  camera={'.'.join(ip.split('.')[:3])}.51:8080")
    ip, how = discover(args.ip); print(f"camera address: {ip}:{args.port}  ({how})")
    print("reachable:", "yes" if Camera(ip, args.port).reachable() else "no")
    for c in find_sd_cards(): print(f"SD card mounted: {c}")


def cmd_status(args):
    cam, dcim = connect(args)
    files = sd_media_list(dcim) if dcim else cam.media_list()
    if cam:
        info = cam.info(); print(f"{info.get('model_name', 'GoPro')}  firmware {info.get('firmware_version', '?')}  serial {info.get('serial_number', '?')}  at {cam.ip}")
        try:
            st = cam.state().get("status", {}); rem = st.get("54")
            print(f"  battery {st.get('70')}%   free {human(int(rem) * 1024) if rem else '?'}   {'RECORDING' if st.get('10') else 'idle'}")
        except Exception as e: print(f"  (state unavailable: {e})")
    vids = [f for f in files if f["kind"] == "video"]; pics = [f for f in files if f["kind"] == "photo"]
    print(f"  {len(vids)} videos ({human(sum(f['size'] for f in vids))}), {len(pics)} photos ({human(sum(f['size'] for f in pics))})")


def cmd_ls(args):
    cam, dcim = connect(args); files = filter_files(sd_media_list(dcim) if dcim else cam.media_list(), args)
    print(f"{'DATE':16}  {'NAME':14}  {'TYPE':6}  {'SIZE':>10}  NOTE"); last = None
    for f in files:
        day = f["created"].strftime("%Y-%m-%d") if f["created"] else "?"
        if last is not None and day != last: print()
        last = day; note = ""
        if f["kind"] == "video" and f["chapter"] and f["chapter"] > 1: note = f"chapter {f['chapter']} of #{f['number']:04d}"
        elif f["group_type"]: note = {"b": "burst", "c": "continuous", "n": "night lapse", "t": "time lapse"}.get(f["group_type"], "group")
        print(f"{(f['created'].strftime('%Y-%m-%d %H:%M') if f['created'] else '?'):16}  {f['name']:14}  {f['kind']:6}  {human(f['size']):>10}  {note}")
    print(f"\n{len(files)} files, {human(sum(f['size'] for f in files))}")


def cmd_pull(args):
    cam, dcim = connect(args); files = filter_files(sd_media_list(dcim) if dcim else cam.media_list(), args, args.names)
    dest_root = os.path.expanduser(args.dest); plan, skipped = [], 0
    for f in files:
        sub = "" if args.flat else (f["created"].strftime("%Y-%m-%d") if f["created"] else "undated")
        target = os.path.join(dest_root, sub, f["name"])
        if os.path.exists(target) and (f["size_estimated"] or os.path.getsize(target) == f["size"]): skipped += 1; continue
        plan.append((f, target))
    total = sum(f["size"] for f, _ in plan)
    print(f"{len(plan)} files to import ({human(total)}), {skipped} already on this Mac -> {dest_root}")
    if args.dry_run:
        for f, t in plan: print(f"  would copy {f['path']:26} -> {t}")
        return
    if not plan: return
    if cam: cam.start_keepalive(); cam.turbo(True)
    done_bytes, failed, t_all = 0, [], time.time()
    try:
        for i, (f, target) in enumerate(plan, 1):
            os.makedirs(os.path.dirname(target), exist_ok=True); label = f"[{i}/{len(plan)}] {f['name']}"
            def progress(done, tot, el, moved=None, _l=label):
                rate = (done if moved is None else moved) / el if el > 0 else 0
                sys.stdout.write(f"\r  {_l:32} {100 * done / tot if tot else 0:5.1f}%  {human(done):>10}  {human(rate)}/s   "); sys.stdout.flush()
            t0 = time.time()
            try:
                if dcim:
                    tmp = target + ".part"; shutil.copy2(f["local"], tmp); os.replace(tmp, target); size = os.path.getsize(target); progress(size, size, time.time() - t0)
                else:
                    cam.lenient = f["size_estimated"]; size = cam.download(f["path"], target, None if f["size_estimated"] else f["size"], progress)
                    if f["created"]: ts = f["created"].timestamp(); os.utime(target, (ts, ts))
                sys.stdout.write("\n"); done_bytes += size
                if args.delete_after:
                    (os.remove(f["local"]) if dcim else cam.delete(f["path"])); print(f"      deleted from camera: {f['name']}")
            except KeyboardInterrupt: print("\ninterrupted; partial kept, rerun to resume"); raise SystemExit(130)
            except Exception as e: sys.stdout.write("\n"); print(f"      FAILED {f['name']}: {e}"); failed.append(f["name"])
    finally:
        if cam: cam.turbo(False); cam.stop_keepalive()
    el = time.time() - t_all
    print(f"\nimported {len(plan) - len(failed)} files, {human(done_bytes)} in {el:,.0f}s ({human(done_bytes / el if el else 0)}/s) -> {dest_root}")
    if failed: print(f"failed: {', '.join(failed)}"); sys.exit(1)


def cmd_rm(args):
    cam, dcim = connect(args); args.include_lrv = True
    targets = filter_files(sd_media_list(dcim) if dcim else cam.media_list(), args, args.names)
    if not targets: die("no matching files")
    for f in targets: print(f"  {f['path']}  {human(f['size'])}")
    if not args.yes and input(f"delete {len(targets)} file(s) from the camera? [y/N] ").strip().lower() != "y": print("cancelled"); return
    for f in targets: (os.remove(f["local"]) if dcim else cam.delete(f["path"])); print(f"deleted {f['path']}")


def cmd_open(args):
    d = os.path.expanduser(args.dest); os.makedirs(d, exist_ok=True); subprocess.run(["open", d])


def main(argv=None):
    p = argparse.ArgumentParser(prog="gopro-py", description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--ip"); p.add_argument("--port", type=int, default=8080); p.add_argument("--sd"); p.add_argument("--dest", default=DEFAULT_DEST)
    sub = p.add_subparsers(dest="cmd", required=True)
    sub.add_parser("find").set_defaults(fn=cmd_find); sub.add_parser("status").set_defaults(fn=cmd_status)
    s = sub.add_parser("ls"); s.set_defaults(fn=cmd_ls); s.add_argument("--since"); s.add_argument("--all", dest="include_lrv", action="store_true")
    s = sub.add_parser("pull"); s.set_defaults(fn=cmd_pull); s.add_argument("names", nargs="*"); s.add_argument("--since")
    s.add_argument("--dry-run", action="store_true"); s.add_argument("--delete-after", action="store_true"); s.add_argument("--include-lrv", action="store_true"); s.add_argument("--flat", action="store_true")
    s = sub.add_parser("rm"); s.set_defaults(fn=cmd_rm); s.add_argument("names", nargs="+"); s.add_argument("-y", "--yes", action="store_true")
    sub.add_parser("open").set_defaults(fn=cmd_open)
    args = p.parse_args(argv)
    try: args.fn(args)
    except KeyboardInterrupt: sys.exit(130)
    except urllib.error.URLError as e: die(f"camera connection failed: {e.reason}")


if __name__ == "__main__":
    main()
