//! The daemon proper: single instance, config, engine, message thread, coordinator.

use crossbeam_channel::unbounded;
use vox_platform_win::{message_box, message_thread, MessageKind, SingleInstance};

use crate::coordinator::{Channels, Coordinator};
use crate::engine::{engine_config, load_engine, spawn_inference};
use crate::paths;

/// Blocks until the user quits from the tray.
pub fn run() -> anyhow::Result<()> {
    let Some(_instance) = SingleInstance::acquire()? else {
        message_box(
            "Vox",
            "Vox is already running — look for its icon in the tray.",
            MessageKind::Info,
        );
        return Ok(());
    };

    let cfg = paths::load_or_create_config()?;
    tracing::info!("config: {}", paths::config_path().display());

    // Fail fast on a missing model, before any tray icon appears.
    let preloaded = if cfg.engine.preload {
        Some(load_engine(&cfg)?)
    } else {
        engine_config(&cfg)?;
        None
    };

    let (hotkey_tx, hotkey_rx) = unbounded();
    let (tray_tx, tray_rx) = unbounded();
    let (audio_tx, audio_rx) = unbounded();
    let (jobs_tx, jobs_rx) = unbounded();
    let (results_tx, results_rx) = unbounded();

    spawn_inference(cfg.clone(), preloaded, jobs_rx, results_tx);

    let msg = message_thread::spawn(message_thread::Options {
        chord: cfg.hotkey.chord,
        hotkey_tx,
        tray_tx,
        tooltip: "Vox - starting".into(),
    })?;

    let channels = Channels {
        hotkey_rx,
        audio_tx,
        audio_rx,
        jobs_tx,
        results_rx,
        tray_rx,
    };
    Coordinator::new(cfg, channels, msg).run();
    Ok(())
}
