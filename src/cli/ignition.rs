use std::path::PathBuf;

use tracing::info;

use crate::ignition::{ButaneSource, IgnitionBuilder};

#[allow(clippy::too_many_arguments)]
pub async fn run(
    sources: Vec<PathBuf>,
    base: Option<PathBuf>,
    overlays: Vec<PathBuf>,
    vars: Vec<String>,
    files_dir: Option<PathBuf>,
    butane: PathBuf,
    output: PathBuf,
    work_dir: &std::path::Path,
) -> eyre::Result<()> {
    let ign_work = work_dir.join("ignition");
    let mut builder = IgnitionBuilder::new(butane, &ign_work);

    if let Some(dir) = files_dir {
        builder = builder.files_dir(dir);
    }

    if let Some(base_ign) = base {
        builder = builder.base_ign(base_ign);
    }

    for var_str in &vars {
        let (key, value) = var_str
            .split_once('=')
            .ok_or_else(|| eyre::eyre!("invalid var format: {var_str} (expected KEY=VALUE)"))?;
        builder = builder.var(key, value);
    }

    for src in &sources {
        builder = builder.source(ButaneSource::File(src.clone()));
    }

    for overlay in &overlays {
        builder = builder.overlay(ButaneSource::File(overlay.clone()));
    }

    let ign_path = builder.build().await?;

    // Copy to requested output location
    tokio::fs::copy(&ign_path, &output).await?;

    info!(output = %output.display(), "Ignition config written");
    Ok(())
}
