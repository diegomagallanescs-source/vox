//! The daemon proper: single instance, config, engine, hook thread, coordinator thread.

use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crossbeam_channel::{unbounded, Sender};
use vox_platform_win::{message_thread, SingleInstance};

use crate::coordinator::{Channels, Coordinator};
use crate::engine::{engine_config, load_engine, spawn_inference};
use crate::paths;
use crate::status::{Status, StatusSink, UiRequest};

/// Handle to the running daemon. Dropping it (or calling [`Daemon::quit`]) stops it.
pub struct Daemon {
    pub ui_tx: Sender<UiRequest>,
    pub status: Arc<Mutex<Status>>,
    _instance: SingleInstance,
    join: Option<JoinHandle<()>>,
}

impl Daemon {
    pub fn quit(&mut self) {
        let _ = self.ui_tx.send(UiRequest::Quit);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        self.quit();
    }
}

/// Starts everything on background threads. `Ok(None)` means another instance already runs.
pub fn start(sink: Arc<dyn StatusSink>) -> anyhow::Result<Option<Daemon>> {
    let Some(instance) = SingleInstance::acquire()? else {
        return Ok(None);
    };

    let cfg = paths::load_or_create_config()?;
    tracing::info!("config: {}", paths::config_path().display());
    let status = Arc::new(Mutex::new(Status::from_config(&cfg)));

    // A missing model is reported through status rather than aborting: the UI can fix it.
    let preloaded = if cfg.engine.preload {
        match load_engine(&cfg) {
            Ok(e) => Some(e),
            Err(e) => {
                tracing::error!("{e:#}");
                status.lock().unwrap().last_error = Some(format!("{e:#}"));
                None
            }
        }
    } else {
        if let Err(e) = engine_config(&cfg) {
            status.lock().unwrap().last_error = Some(format!("{e:#}"));
        }
        None
    };

    let (hotkey_tx, hotkey_rx) = unbounded();
    let (audio_tx, audio_rx) = unbounded();
    let (inference_tx, inference_rx) = unbounded();
    let (events_tx, events_rx) = unbounded();
    let (ui_tx, ui_rx) = unbounded();

    spawn_inference(cfg.clone(), preloaded, inference_rx, events_tx);

    let hooks = message_thread::spawn(message_thread::Options {
        chord: cfg.hotkey.chord,
        hotkey_tx,
    })?;

    let channels = Channels {
        hotkey_rx,
        audio_tx,
        audio_rx,
        inference_tx,
        events_rx,
        ui_rx,
    };
    let coordinator = Coordinator::new(cfg, channels, hooks, status.clone(), sink);
    let join = std::thread::Builder::new()
        .name("vox-coordinator".into())
        .spawn(move || coordinator.run())?;

    Ok(Some(Daemon {
        ui_tx,
        status,
        _instance: instance,
        join: Some(join),
    }))
}
