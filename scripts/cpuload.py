#!/usr/bin/env python3
"""Measure CPU (and GPU) load of running programs, for comparing kalila with the stock GUI.

    scripts/cpuload.py --name kalila [--name basler_record --name manta_record ...]
                       [--seconds 120] [--interval 1] [--csv out.csv] [--label "kalila recording"]

Each --name matches processes whose command name (comm) or executable basename equals it,
or starts with it (e.g. `preview` matches the camera previews); all matches form one group.
--pid P adds one process by PID as its own group. Processes that start or exit during the
run are picked up / dropped every interval.

Reported per group, in % of ONE core (100 % = one core fully busy; this machine has
`nproc` cores): mean, median, p95 and peak over the intervals, plus RSS and the busiest
threads. Also whole-system busy % (of all cores), the busiest core per interval, and, if
nvidia-smi exists, GPU utilisation / encoder / graphics clock / power. Stdlib only.
"""
import argparse
import csv
import os
import shutil
import statistics as st
import subprocess
import sys
import time

TCK = os.sysconf("SC_CLK_TCK")
NCPU = os.cpu_count() or 1


def read(path):
    try:
        with open(path, "rb") as f:
            return f.read().decode(errors="replace")
    except OSError:
        return None


def proc_ticks(stat):
    # Fields after the ")" of comm: state is [0]; utime, stime are fields 14, 15 → [11], [12].
    rest = stat[stat.rindex(")") + 2:].split()
    return int(rest[11]) + int(rest[12])


def comm_of(stat):
    return stat[stat.index("(") + 1:stat.rindex(")")]


def matches(pid, names):
    comm = (read(f"/proc/{pid}/comm") or "").strip()
    try:
        exe = os.path.basename(os.readlink(f"/proc/{pid}/exe"))
    except OSError:
        exe = ""
    return any(c == n or c.startswith(n) for n in names for c in (comm, exe) if c)


def find(names):
    me = os.getpid()
    return [int(p) for p in os.listdir("/proc") if p.isdigit() and int(p) != me and matches(int(p), names)]


def threads(pid):
    out = {}
    try:
        tids = os.listdir(f"/proc/{pid}/task")
    except OSError:
        return out
    for t in tids:
        s = read(f"/proc/{pid}/task/{t}/stat")
        if s:
            out[(pid, int(t))] = (comm_of(s), proc_ticks(s))
    return out


def rss_mb(pid):
    s = read(f"/proc/{pid}/status") or ""
    for line in s.splitlines():
        if line.startswith("VmRSS:"):
            return int(line.split()[1]) / 1024
    return 0.0


def cpu_times():
    """Per-CPU (busy, total) jiffies from /proc/stat, index 0 = aggregate."""
    out = []
    for line in (read("/proc/stat") or "").splitlines():
        if not line.startswith("cpu"):
            break
        v = [int(x) for x in line.split()[1:]]
        idle = v[3] + v[4]  # idle + iowait
        total = sum(v[:8])  # exclude guest (already counted in user)
        out.append((total - idle, total))
    return out


def gpu_sample():
    if not shutil.which("nvidia-smi"):
        return None
    try:
        r = subprocess.run(["nvidia-smi", "--query-gpu=utilization.gpu,utilization.encoder,clocks.gr,power.draw",
                            "--format=csv,noheader,nounits"], capture_output=True, text=True, timeout=2)
        g, e, c, p = (x.strip() for x in r.stdout.splitlines()[0].split(","))
        return float(g), float(e), float(c), float(p)
    except Exception:
        return None


def pct(xs, q):
    if not xs:
        return 0.0
    s = sorted(xs)
    return s[min(len(s) - 1, int(round(q * (len(s) - 1))))]


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--name", action="append", default=[], help="process name (repeatable; each one is its own group)")
    ap.add_argument("--pid", action="append", type=int, default=[], help="process id (repeatable)")
    ap.add_argument("--seconds", type=float, default=120)
    ap.add_argument("--interval", type=float, default=1.0)
    ap.add_argument("--csv", help="write per-interval samples here")
    ap.add_argument("--label", default="", help="free text printed in the summary header")
    ap.add_argument("--gpu", action=argparse.BooleanOptionalAction, default=True, help="sample nvidia-smi (default on)")
    a = ap.parse_args()
    groups = [(n, lambda n=n: find([n])) for n in a.name] + [(f"pid {p}", lambda p=p: [p] if os.path.exists(f"/proc/{p}") else []) for p in a.pid]
    if not groups:
        ap.error("give at least one --name or --pid")
    for g, f in groups:
        pids = f()
        print(f"{g}: {', '.join(map(str, pids)) if pids else 'NOT RUNNING (yet)'}", file=sys.stderr)

    hist = {g: [] for g, _ in groups}
    rss = {g: [] for g, _ in groups}
    th_total = {}  # (group, thread name, tid) -> ticks over the run
    sys_busy, core_max, gpu = [], [], []
    rows = []
    prev_th = {g: {} for g, _ in groups}
    for g, f in groups:
        for p in f():
            prev_th[g].update(threads(p))
    prev_cpu = cpu_times()
    t_prev = time.monotonic()
    t_end = t_prev + a.seconds
    n = 0
    while time.monotonic() < t_end:
        time.sleep(max(0.0, t_prev + a.interval - time.monotonic()))
        t_now = time.monotonic()
        dt = t_now - t_prev
        t_prev = t_now
        row = {"t": round(a.interval * (n + 1), 3)}
        for g, f in groups:
            cur = {}
            pids = f()
            for p in pids:
                cur.update(threads(p))
            used = 0
            for k, (name, ticks) in cur.items():
                if k in prev_th[g]:
                    d = ticks - prev_th[g][k][1]
                    used += d
                    th_total[(g, name, k[1])] = th_total.get((g, name, k[1]), 0) + d
            prev_th[g] = cur
            p100 = 100.0 * used / TCK / dt
            hist[g].append(p100)
            r = sum(rss_mb(p) for p in pids)
            rss[g].append(r)
            row[f"{g} cpu%"] = round(p100, 2)
            row[f"{g} rss MB"] = round(r, 1)
        c = cpu_times()
        busy = [(c[i][0] - prev_cpu[i][0]) / max(1, c[i][1] - prev_cpu[i][1]) * 100 for i in range(len(c))]
        prev_cpu = c
        sys_busy.append(busy[0])
        core_max.append(max(busy[1:]) if len(busy) > 1 else busy[0])
        row["system busy%"] = round(busy[0], 2)
        row["busiest core%"] = round(core_max[-1], 2)
        if a.gpu:
            s = gpu_sample()
            if s:
                gpu.append(s)
                row.update({"gpu%": s[0], "enc%": s[1], "gpu MHz": s[2], "gpu W": s[3]})
        rows.append(row)
        n += 1
        if n % 10 == 0:
            print(f"  {row['t']:.0f} s: " + "  ".join(f"{g} {hist[g][-1]:.1f}%" for g, _ in groups) + f"  system {busy[0]:.1f}%", file=sys.stderr)

    if a.csv and rows:
        with open(a.csv, "w", newline="") as f:
            w = csv.DictWriter(f, fieldnames=list(dict.fromkeys(k for r in rows for k in r)))
            w.writeheader()
            w.writerows(rows)

    print(f"\n== {a.label or 'cpuload'}: {n} × {a.interval:g} s, {NCPU} cores (100 % = one core) ==")
    print(f"{'group':<22}{'mean':>8}{'median':>8}{'p95':>8}{'peak':>8}{'RSS MB':>9}")
    for g, _ in groups:
        h = hist[g]
        print(f"{g:<22}{st.mean(h):8.1f}{st.median(h):8.1f}{pct(h, 0.95):8.1f}{max(h):8.1f}{(st.mean(rss[g]) if rss[g] else 0):9.0f}")
        # Threads with the same name (e.g. unnamed threads inheriting the program's name) are
        # listed separately, by thread id.
        mine = [(v, name, tid) for (gg, name, tid), v in th_total.items() if gg == g]
        dup = {name for _, name, _ in mine if sum(1 for _, nm, _ in mine if nm == name) > 1}
        top = sorted(mine, reverse=True)[:8]
        if top and n:
            secs = n * a.interval
            print("    threads: " + ", ".join(f"{name}{f'#{tid}' if name in dup else ''} {100.0 * v / TCK / secs:.1f}" for v, name, tid in top if v > 0))
    print(f"{'system (all cores)':<22}{st.mean(sys_busy):8.1f}{st.median(sys_busy):8.1f}{pct(sys_busy, 0.95):8.1f}{max(sys_busy):8.1f}   % of {NCPU} cores")
    print(f"{'busiest core':<22}{st.mean(core_max):8.1f}{st.median(core_max):8.1f}{pct(core_max, 0.95):8.1f}{max(core_max):8.1f}")
    if gpu:
        for i, name in enumerate(["GPU util %", "encoder %", "graphics MHz", "power W"]):
            xs = [s[i] for s in gpu]
            print(f"{name:<22}{st.mean(xs):8.1f}{st.median(xs):8.1f}{pct(xs, 0.95):8.1f}{max(xs):8.1f}")


if __name__ == "__main__":
    main()
