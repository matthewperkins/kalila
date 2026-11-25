//! SIGTERM and SIGINT must finalise an open recording.
//!
//! Runs the `headless` example on the simulator (never `--hw`, always `--no-audio`), waits
//! until it is recording, signals it, and checks the files: exit status 0, `structure.oebin`
//! is JSON, and for the main stream the `.npy` header counts exactly the records in
//! `continuous.dat`, with contiguous sample numbers. Without the handler the headers are up
//! to a second stale and the buffered tail is missing.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn headless_binary() -> PathBuf {
    // target/<profile>/deps/signals-xxxx  ->  target/<profile>/examples/headless
    let exe = std::env::current_exe().unwrap();
    let p = exe
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("examples")
        .join("headless");
    assert!(
        p.exists(),
        "{} not built: cargo builds examples only when the whole package is tested (`cargo test -p engine`, not `--test signals`)",
        p.display()
    );
    p
}

fn walk(d: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(d).unwrap().flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else {
            out.push(p);
        }
    }
}

/// (record count from the header, body bytes) of a 1-D `.npy`.
fn npy(path: &Path) -> (usize, Vec<u8>) {
    let b = std::fs::read(path).unwrap();
    assert_eq!(&b[..8], b"\x93NUMPY\x01\x00", "{}", path.display());
    let hl = u16::from_le_bytes([b[8], b[9]]) as usize;
    let h = String::from_utf8(b[10..10 + hl].to_vec()).unwrap();
    let n: usize = h
        .split("'shape': (")
        .nth(1)
        .and_then(|s| s.split(',').next())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or_else(|| panic!("no shape in {h}"));
    (n, b[10 + hl..].to_vec())
}

fn run(sig: libc::c_int) {
    let dir = std::env::temp_dir().join(format!("kalila-signals-{}-{sig}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut child = Command::new(headless_binary())
        .args(["--no-audio", "--seconds", "60", "--record"])
        .arg(&dir)
        .env("RUST_LOG", "warn")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id() as libc::pid_t;
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for l in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(l).is_err() {
                break;
            }
        }
    });
    // Recording once a status line reports at least 2 MB written.
    let t0 = Instant::now();
    let mut channels = None;
    loop {
        if let Ok(l) = rx.recv_timeout(Duration::from_millis(200)) {
            if let Some(rest) = l.split("| rec ").nth(1) {
                let mb: u64 = rest.split_whitespace().next().unwrap().parse().unwrap();
                if mb >= 2 {
                    break;
                }
            }
            if channels.is_none() && l.contains(" channels @ ") {
                channels = l.split_whitespace().next().and_then(|c| c.parse::<usize>().ok());
            }
        }
        if t0.elapsed() > Duration::from_secs(30) {
            let _ = child.kill();
            panic!("headless never reported recording");
        }
    }
    unsafe { libc::kill(pid, sig) };
    let t1 = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait().unwrap() {
            break s;
        }
        if t1.elapsed() > Duration::from_secs(10) {
            unsafe { libc::kill(pid, libc::SIGKILL) };
            let _ = child.wait();
            panic!("headless did not exit within 10 s of signal {sig}");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(status.success(), "exit status {status:?} after signal {sig}");
    let channels = channels.expect("channel count line");

    let mut files = Vec::new();
    walk(&dir, &mut files);
    let one = |name: &str, under: &str| -> PathBuf {
        let v: Vec<_> = files
            .iter()
            .filter(|p| p.file_name().is_some_and(|n| n == name) && p.to_string_lossy().contains(under))
            .collect();
        assert_eq!(v.len(), 1, "{name} under {under}: {v:?}");
        v[0].clone()
    };
    let oebin = one("structure.oebin", "");
    serde_json::from_slice::<serde_json::Value>(&std::fs::read(&oebin).unwrap()).expect("structure.oebin is JSON");
    let dat = one("continuous.dat", "continuous");
    let (n, sn) = npy(&one("sample_numbers.npy", "continuous"));
    assert!(n > 30_000, "only {n} records");
    let size = std::fs::metadata(&dat).unwrap().len() as usize;
    assert_eq!(
        n * channels * 2,
        size,
        "header says {n} records x {channels} ch; continuous.dat has {size} bytes"
    );
    assert_eq!(sn.len(), n * 8, "sample_numbers.npy body");
    let nums: Vec<i64> = sn
        .chunks_exact(8)
        .map(|c| i64::from_le_bytes(c.try_into().unwrap()))
        .collect();
    for (i, w) in nums.windows(2).enumerate() {
        assert_eq!(w[1], w[0] + 1, "sample numbers not contiguous at record {i}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sigterm_finalises_the_recording() {
    run(libc::SIGTERM);
}

#[test]
fn sigint_finalises_the_recording() {
    run(libc::SIGINT);
}
