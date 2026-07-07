//! Thin CLI wrapper around the `lanpilot_agent` library.
//!
//! Resolves configuration from environment variables and stdin (preserving
//! the previous interactive CLI behavior), then runs the agent connection
//! flow to completion on this thread. The stop flag is never set here since
//! there is no interactive cancel button on the CLI.

use std::io::{self, Write};

use lanpilot_agent::{AgentConfig, run_agent};
use lanpilot_core::{Logger, new_stop_flag, normalize_pair_code};

fn main() -> Result<(), String> {
    let agent_name = std::env::var("COMPUTERNAME").ok();
    let pair_code = read_pair_code()?;

    let render_enabled = std::env::var("LANPILOT_RENDER")
        .map(|raw| raw != "0")
        .unwrap_or(true);
    let target_stream_frames = std::env::var("LANPILOT_STREAM_FRAMES")
        .ok()
        .and_then(|raw| raw.parse::<u32>().ok())
        .unwrap_or(60);

    let config = AgentConfig {
        agent_name,
        pair_code,
        render_enabled,
        target_stream_frames,
        preferred_host_ipv4: None,
        preferred_host_name: None,
    };

    run_agent(config, Logger::stdout(), new_stop_flag())
}

fn read_pair_code() -> Result<String, String> {
    if let Ok(raw) = std::env::var("LANPILOT_PAIR_CODE") {
        if let Some(code) = normalize_pair_code(&raw) {
            println!("Usando codigo de conexion desde LANPILOT_PAIR_CODE.");
            return Ok(code);
        }
        return Err("LANPILOT_PAIR_CODE debe tener exactamente 6 digitos".to_string());
    }

    for attempt in 1..=3 {
        if attempt == 1 {
            print!("Introduce el codigo de 6 digitos: ");
        } else {
            print!("Código inválido. Debe tener 6 dígitos. Inténtalo de nuevo: ");
        }
        io::stdout()
            .flush()
            .map_err(|err| format!("flush stdout failed: {err}"))?;

        let mut input = String::new();
        io::stdin()
            .read_line(&mut input)
            .map_err(|err| format!("read pair code failed: {err}"))?;
        if let Some(code) = normalize_pair_code(&input) {
            return Ok(code);
        }
    }

    Err("Codigo invalido: usa 6 digitos".to_string())
}
