//! Logging: stderr (visible when a console is attached) plus an append-only file at
//! `%LOCALAPPDATA%\Vox\logs\voxd.log`. `RUST_LOG` controls the level; default `info`.

use std::sync::Mutex;

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

use crate::paths;

pub fn init() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let console = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_target(false);
    let log_path = paths::log_path();
    let file = log_path
        .parent()
        .and_then(|d| std::fs::create_dir_all(d).ok())
        .and_then(|_| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
                .ok()
        })
        .map(|f| {
            tracing_subscriber::fmt::layer()
                .with_writer(Mutex::new(f))
                .with_ansi(false)
                .with_target(false)
        });
    tracing_subscriber::registry()
        .with(filter)
        .with(console)
        .with(file)
        .init();
}
