//! Where the daemon keeps its config, logs, and looks for models.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context};
use vox_core::Config;

fn env_dir(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
}

/// `%APPDATA%\Vox`
pub fn config_dir() -> PathBuf {
    env_dir("APPDATA")
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Vox")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

/// `%LOCALAPPDATA%\Vox`
pub fn local_dir() -> PathBuf {
    env_dir("LOCALAPPDATA")
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Vox")
}

pub fn log_path() -> PathBuf {
    local_dir().join("logs").join("voxd.log")
}

/// Loads the config, writing a default file on first run so the user has something to edit.
pub fn load_or_create_config() -> anyhow::Result<Config> {
    let path = config_path();
    if path.is_file() {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        return Config::from_toml(&text).with_context(|| format!("parsing {}", path.display()));
    }
    let cfg = Config::default();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    std::fs::write(&path, cfg.to_toml()?).with_context(|| format!("writing {}", path.display()))?;
    tracing::info!("wrote default config to {}", path.display());
    Ok(cfg)
}

/// Directories searched for `ggml-<model>.bin`, in order: next to the exe and up to three
/// levels above it (so a dev build in `target\release` finds the repo's `models\`), the
/// current directory, then `%LOCALAPPDATA%\Vox\models`.
pub fn model_search_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut push = |d: PathBuf| {
        if !dirs.contains(&d) {
            dirs.push(d);
        }
    };
    if let Ok(exe) = std::env::current_exe() {
        let mut dir = exe.parent();
        for _ in 0..4 {
            match dir {
                Some(d) => {
                    push(d.join("models"));
                    dir = d.parent();
                }
                None => break,
            }
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        push(cwd.join("models"));
    }
    push(local_dir().join("models"));
    dirs
}

/// Model names available on disk (`ggml-<name>.bin` in any search dir), sorted, de-duplicated.
pub fn list_models() -> Vec<(String, PathBuf)> {
    let mut out: Vec<(String, PathBuf)> = Vec::new();
    for dir in model_search_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if path.extension().is_some_and(|e| e == "bin") {
                if let Some(name) = stem.strip_prefix("ggml-") {
                    if !out.iter().any(|(n, _)| n == name) {
                        out.push((name.to_string(), path.clone()));
                    }
                }
            }
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// `model` is either a name like `base.en-q5_1` or an explicit path to a `.bin`.
pub fn resolve_model(model: &str) -> anyhow::Result<PathBuf> {
    let as_path = Path::new(model);
    if as_path.extension().is_some_and(|e| e == "bin") || model.contains(['/', '\\']) {
        return if as_path.is_file() {
            Ok(as_path.to_path_buf())
        } else {
            Err(anyhow!("model file not found: {}", as_path.display()))
        };
    }
    let file = format!("ggml-{model}.bin");
    let dirs = model_search_dirs();
    for dir in &dirs {
        let candidate = dir.join(&file);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(anyhow!(
        "model `{model}` not found; looked for {file} in:\n{}",
        dirs.iter()
            .map(|d| format!("  {}", d.display()))
            .collect::<Vec<_>>()
            .join("\n")
    ))
}
