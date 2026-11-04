//! Headless acquisition + recording with live health stats (a soak-test tool for the rig).
//!
//! ```text
//! cargo run --release -p engine --example headless -- [--hw] [--seconds N] [--record DIR] [--rate HZ]
//! ```
use engine::{BoardChoice, Engine};
use rhythm::BoardSettings;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args: Vec<String> = std::env::args().collect();
    let flag = |f: &str| args.iter().any(|a| a == f);
    let opt = |f: &str| args.iter().position(|a| a == f).and_then(|i| args.get(i + 1)).cloned();
    let secs: f64 = opt("--seconds").map(|s| s.parse().unwrap()).unwrap_or(10.0);
    let root = opt("--record").map(std::path::PathBuf::from);
    let mut settings = BoardSettings::default();
    if let Some(r) = opt("--rate") {
        settings.sample_rate = r.parse()?;
    }
    let mut e = Engine::new(root.clone().unwrap_or_else(std::env::temp_dir));
    e.audio_settings.lock().enabled = !flag("--no-audio");
    e.open_board(if flag("--hw") { BoardChoice::Hardware } else { BoardChoice::Simulated(vec![32, 32]) }, settings);
    while e.is_busy() {
        e.poll();
        std::thread::sleep(Duration::from_millis(20));
    }
    if let Some(err) = &e.last_error {
        anyhow::bail!("{err}");
    }
    let l = e.layout().unwrap().clone();
    println!("{} channels @ {} Hz; headstages: {:?}", l.num_channels(), l.sample_rate, l.headstages.iter().map(|h| format!("{} {} {}ch", h.name, h.chip, h.num_channels)).collect::<Vec<_>>());
    e.start_acquisition()?;
    println!("real-time priority: {}", e.rt_priority());
    std::thread::sleep(Duration::from_millis(200));
    if root.is_some() {
        e.start_recording();
    }
    let t0 = Instant::now();
    let mut spikes = 0usize;
    let mut last = Instant::now();
    let mut lat_max = Duration::ZERO;
    while t0.elapsed().as_secs_f64() < secs {
        e.poll();
        if !e.is_acquiring() {
            anyhow::bail!("acquisition stopped: {:?}", e.last_error);
        }
        if let Some(rx) = e.display_rx() {
            while let Ok(b) = rx.try_recv() {
                if let Some(t) = b.acquired_at {
                    lat_max = lat_max.max(t.elapsed());
                }
            }
        }
        if let Some(rx) = e.spike_rx() {
            spikes += rx.try_iter().count();
        }
        if last.elapsed() > Duration::from_secs(1) {
            last = Instant::now();
            let h = e.health.snapshot();
            let p = &e.pipeline;
            let a = &e.audio_stats;
            println!(
                "t={:5.1}s samples={} dropped={} gaps={} bad={} hwbuf={:.1}% | drops disp/dsp/aud={}/{}/{} recQ={} bp={}ms | spikes={} | audio {}Hz lat={:.1}ms underruns={} | disp lat max={:.2}ms | rec {} MB",
                t0.elapsed().as_secs_f64(), h.samples, h.dropped_samples, h.discontinuities, h.bad_frames, h.hw_buffer_pct,
                p.display_dropped.load(Ordering::Relaxed), p.dsp_dropped.load(Ordering::Relaxed), p.audio_dropped.load(Ordering::Relaxed),
                p.rec_queue.load(Ordering::Relaxed), p.rec_backpressure_us.load(Ordering::Relaxed) / 1000, spikes,
                a.device_rate.load(Ordering::Relaxed), a.latency_us.load(Ordering::Relaxed) as f64 / 1000.0, a.underruns.load(Ordering::Relaxed),
                lat_max.as_secs_f64() * 1e3, e.rec_status.bytes.load(Ordering::Relaxed) / 1_000_000
            );
            lat_max = Duration::ZERO;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    e.stop_recording();
    std::thread::sleep(Duration::from_millis(100));
    e.stop_acquisition();
    if let Some(d) = e.rec_status.current_dir.lock().clone() {
        println!("recorded to {}", d.display());
    }
    if let Some(err) = e.rec_status.error.lock().clone() {
        anyhow::bail!("recording error: {err}");
    }
    Ok(())
}
