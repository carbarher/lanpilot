//! Thin CLI wrapper around the `lanpilot_host` library.
//!
//! Resolves configuration from environment variables (preserving the
//! previous CLI behavior) and runs the host stack to completion on this
//! thread. The stop flag is never set here, so this mirrors the old
//! "runs forever until the process is killed" behavior.

use std::fs::OpenOptions;
use std::io::Write;
use std::sync::Mutex;

use lanpilot_core::{Logger, new_stop_flag};
use lanpilot_host::{HostConfig, StreamSource, run_host};

fn main() {
    let host_name = std::env::var("COMPUTERNAME").ok();
    let pair_code = std::env::var("LANPILOT_PAIR_CODE").ok();
    let stream_source = match std::env::var("LANPILOT_STREAM_SOURCE") {
        Ok(raw) if raw.eq_ignore_ascii_case("synthetic") => StreamSource::Synthetic,
        _ => StreamSource::Screen,
    };
    let max_stream_frames = std::env::var("LANPILOT_MAX_STREAM_FRAMES")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(u64::MAX);

    let config = HostConfig {
        host_name,
        pair_code,
        stream_source,
        max_stream_frames,
    };

    // Logger that writes to both stdout and a file in the same directory as the executable
    let logger = create_file_logger();

    if let Err(err) = run_host(config, logger, new_stop_flag()) {
        eprintln!("Error: {err}");
        std::process::exit(1);
    }
}

/// Create a logger that writes to both stdout and a file in the executable directory
fn create_file_logger() -> Logger {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    
    let log_path = exe_dir.join("LanPilot-debug.log");
    let log_file = std::sync::Arc::new(Mutex::new(
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .unwrap_or_else(|_| {
                eprintln!("Warning: could not open log file at {}", log_path.display());
                // Fallback to /dev/null equivalent on Windows
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open("nul")
                    .unwrap()
            })
    ));

    Logger::new(move |message| {
        // Always print to stdout
        println!("{message}");
        
        // Also write to log file
        if let Ok(mut file) = log_file.lock() {
            let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
            let _ = writeln!(file, "[{timestamp}] {message}");
            let _ = file.flush();
        }
    })
}
