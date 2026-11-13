//! kalila — Open Ephys acquisition GUI in Rust (egui + wgpu/Vulkan).

mod app;
mod config;
mod notes;
mod slope;
mod review;
mod spikes;
mod sweep;
mod traces;
mod ttl;
mod uiscript;

use eframe::egui_wgpu::{self, wgpu};

fn main() -> eframe::Result {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info,wgpu_core=warn,wgpu_hal=warn,naga=warn")).init();
    let mut cfg = config::Config::load();
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        println!("kalila [--sim] [--acquire]\n  --sim      open the simulated board instead of hardware\n  --acquire  start acquiring as soon as the board is open\nConfig: {}", config::Config::path().display());
        return Ok(());
    }
    if args.iter().any(|a| a == "--sim") {
        cfg.open_hardware_on_start = false;
    }
    let autostart = args.iter().any(|a| a == "--acquire");
    let present_mode = match cfg.present_mode.as_str() {
        "vsync" => wgpu::PresentMode::AutoVsync,
        "fifo" => wgpu::PresentMode::Fifo,
        "mailbox" => wgpu::PresentMode::Mailbox,
        "immediate" => wgpu::PresentMode::Immediate,
        _ => wgpu::PresentMode::AutoNoVsync, // Immediate → Mailbox → Fifo, chosen by wgpu
    };
    let mut wgpu_options = egui_wgpu::WgpuConfiguration::default().with_surface_config(egui_wgpu::SurfaceConfig { present_mode, desired_maximum_frame_latency: Some(1) });
    if let egui_wgpu::WgpuSetup::CreateNew(ref mut c) = wgpu_options.wgpu_setup {
        c.instance_descriptor.backends = wgpu::Backends::VULKAN | wgpu::Backends::GL;
        c.power_preference = wgpu::PowerPreference::HighPerformance;
    }
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Wgpu,
        wgpu_options,
        viewport: {
            let v = eframe::egui::ViewportBuilder::default().with_title("kalila — Open Ephys acquisition").with_app_id("kalila").with_inner_size([1600.0, 1000.0]);
            // Window icon: Bastet and Sekhmet (packaging/icon; credits in ICON-CREDITS.md).
            match eframe::icon_data::from_png_bytes(include_bytes!("../../packaging/icon/kalila-128.png")) {
                Ok(icon) => v.with_icon(std::sync::Arc::new(icon)),
                Err(_) => v,
            }
        },
        ..Default::default()
    };
    eframe::run_native("kalila", options, Box::new(move |cc| Ok(Box::new(app::App::new(cc, cfg, autostart)))))
}
