//! Thin CLI wrapper around the `lanpilot_host` library.
//!
//! Resolves configuration from environment variables (preserving the
//! previous CLI behavior) and runs the host stack to completion on this
//! thread. The stop flag is never set here, so this mirrors the old
//! "runs forever until the process is killed" behavior.

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

    if let Err(err) = run_host(config, Logger::stdout(), new_stop_flag()) {
        eprintln!("Error: {err}");
        std::process::exit(1);
    }
}
