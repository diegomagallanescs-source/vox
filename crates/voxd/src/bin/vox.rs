//! vox.exe — console CLI for diagnostics.
//!
//! ```text
//! vox info             config / log / model locations and the configured hotkey
//! vox devices          capture devices and which are the Windows defaults
//! vox mic-test [SECS]  record from the configured mic for SECS (default 4) and transcribe
//! vox transcribe WAV   run a 16 kHz WAV through the engine
//! ```

use anyhow::anyhow;

const USAGE: &str = "vox info | vox devices | vox mic-test [SECS] | vox transcribe FILE.wav";

fn main() {
    voxd::logging::init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("info") => voxd::cli::info(),
        Some("devices") => voxd::cli::devices(),
        Some("mic-test") => {
            voxd::cli::mic_test(args.get(1).and_then(|s| s.parse().ok()).unwrap_or(4))
        }
        Some("transcribe") => match args.get(1) {
            Some(path) => voxd::cli::transcribe(path),
            None => Err(anyhow!("transcribe needs a WAV path\n{USAGE}")),
        },
        None | Some("-h" | "--help") => {
            println!("{USAGE}");
            Ok(())
        }
        Some(other) => Err(anyhow!("unknown command `{other}`\n{USAGE}")),
    };
    if let Err(e) = result {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
