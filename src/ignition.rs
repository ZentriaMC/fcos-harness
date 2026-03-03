use std::collections::HashMap;
use std::path::{Path, PathBuf};

use eyre::{Context, bail};
use minijinja::Environment;
use tokio::process::Command;
use tracing::{debug, info};

/// A source for Butane configuration.
#[derive(Debug, Clone)]
pub enum ButaneSource {
    /// A .bu file path.
    File(PathBuf),
    /// Inline Butane YAML content.
    Inline(String),
}

/// Builder for constructing an Ignition config from Butane sources.
pub struct IgnitionBuilder {
    butane_bin: PathBuf,
    files_dir: Option<PathBuf>,
    template_vars: HashMap<String, String>,
    base_ign: Option<PathBuf>,
    sources: Vec<ButaneSource>,
    overlays: Vec<ButaneSource>,
    work_dir: PathBuf,
}

impl IgnitionBuilder {
    pub fn new(butane_bin: impl Into<PathBuf>, work_dir: impl Into<PathBuf>) -> Self {
        Self {
            butane_bin: butane_bin.into(),
            files_dir: None,
            template_vars: HashMap::new(),
            base_ign: None,
            sources: Vec::new(),
            overlays: Vec::new(),
            work_dir: work_dir.into(),
        }
    }

    /// Set the `--files-dir` for butane compilation.
    pub fn files_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.files_dir = Some(dir.into());
        self
    }

    /// Set a pre-compiled .ign file as the merge base.
    /// Overlays will be merged on top of this.
    pub fn base_ign(mut self, path: impl Into<PathBuf>) -> Self {
        self.base_ign = Some(path.into());
        self
    }

    /// Add a minijinja template variable.
    pub fn var(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.template_vars.insert(key.into(), value.into());
        self
    }

    /// Bulk-add template variables.
    pub fn vars(mut self, vars: HashMap<String, String>) -> Self {
        self.template_vars.extend(vars);
        self
    }

    /// Add a primary Butane source (compiled and merged in order).
    pub fn source(mut self, source: ButaneSource) -> Self {
        self.sources.push(source);
        self
    }

    /// Add an overlay .bu (compiled separately and merged on top via Ignition merge).
    pub fn overlay(mut self, source: ButaneSource) -> Self {
        self.overlays.push(source);
        self
    }

    /// Compile all sources + overlays into a single Ignition JSON file.
    /// Returns the path to the final .ign file.
    pub async fn build(&self) -> eyre::Result<PathBuf> {
        tokio::fs::create_dir_all(&self.work_dir).await?;

        let mut compiled_igns: Vec<PathBuf> = Vec::new();

        // If a pre-compiled base .ign is provided, copy it into work_dir
        if let Some(ref base) = self.base_ign {
            let dest = self.work_dir.join("base.ign");
            tokio::fs::copy(base, &dest)
                .await
                .wrap_err_with(|| format!("failed to copy base .ign from {}", base.display()))?;
            compiled_igns.push(dest);
        }

        // Compile each primary .bu source
        for (i, source) in self.sources.iter().enumerate() {
            let ign = self.compile_source(source, &format!("source-{i}")).await?;
            compiled_igns.push(ign);
        }

        // Compile each overlay .bu
        let mut overlay_igns: Vec<PathBuf> = Vec::new();
        for (i, source) in self.overlays.iter().enumerate() {
            let ign = self.compile_source(source, &format!("overlay-{i}")).await?;
            overlay_igns.push(ign);
        }

        if compiled_igns.is_empty() && overlay_igns.is_empty() {
            bail!(
                "no ignition sources provided (use --base, positional .bu sources, or --overlay)"
            );
        }

        // If we have exactly one source and no overlays, just return it
        if compiled_igns.len() == 1 && overlay_igns.is_empty() {
            let final_path = self.work_dir.join("config.ign");
            tokio::fs::copy(&compiled_igns[0], &final_path).await?;
            return Ok(final_path);
        }

        // Generate a merge wrapper Butane config
        let merge_bu = self.generate_merge_config(&compiled_igns, &overlay_igns)?;
        let merge_bu_path = self.work_dir.join("merge.bu");
        tokio::fs::write(&merge_bu_path, &merge_bu).await?;

        let final_path = self.work_dir.join("config.ign");
        self.run_butane(&merge_bu_path, &final_path).await?;

        info!(path = %final_path.display(), "Ignition config built");
        Ok(final_path)
    }

    /// Template and compile a single Butane source to .ign.
    async fn compile_source(&self, source: &ButaneSource, name: &str) -> eyre::Result<PathBuf> {
        let bu_content = match source {
            ButaneSource::File(path) => tokio::fs::read_to_string(path)
                .await
                .wrap_err_with(|| format!("failed to read {}", path.display()))?,
            ButaneSource::Inline(content) => content.clone(),
        };

        // Apply minijinja templating if we have template vars
        let rendered = if self.template_vars.is_empty() {
            bu_content
        } else {
            self.render_template(&bu_content, name)?
        };

        let bu_path = self.work_dir.join(format!("{name}.bu"));
        tokio::fs::write(&bu_path, &rendered).await?;

        let ign_path = self.work_dir.join(format!("{name}.ign"));
        self.run_butane(&bu_path, &ign_path).await?;

        Ok(ign_path)
    }

    /// Render a Butane file through minijinja.
    fn render_template(&self, content: &str, name: &str) -> eyre::Result<String> {
        let mut env = Environment::new();
        env.add_template(name, content)
            .wrap_err("failed to parse Butane template")?;

        let tmpl = env.get_template(name).unwrap();
        let rendered = tmpl
            .render(&self.template_vars)
            .wrap_err("failed to render Butane template")?;

        Ok(rendered)
    }

    /// Run butane to compile a .bu file to .ign.
    async fn run_butane(&self, input: &Path, output: &Path) -> eyre::Result<()> {
        debug!(
            input = %input.display(),
            output = %output.display(),
            "compiling Butane"
        );

        let mut cmd = Command::new(&self.butane_bin);
        cmd.arg("--strict");

        if let Some(ref files_dir) = self.files_dir {
            cmd.arg("--files-dir").arg(files_dir);
        }

        let input_content = tokio::fs::read(input)
            .await
            .wrap_err_with(|| format!("failed to read {}", input.display()))?;

        let result = cmd
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .wrap_err("failed to spawn butane")?;

        // Write stdin and collect output
        let mut child = result;
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            stdin.write_all(&input_content).await?;
            // Drop to close stdin
        }

        let output_result = child
            .wait_with_output()
            .await
            .wrap_err("butane process failed")?;

        if !output_result.status.success() {
            let stderr = String::from_utf8_lossy(&output_result.stderr);
            bail!(
                "butane compilation failed for {}: {stderr}",
                input.display()
            );
        }

        tokio::fs::write(output, &output_result.stdout).await?;
        Ok(())
    }

    /// Generate a Butane merge config that combines multiple .ign files.
    fn generate_merge_config(
        &self,
        sources: &[PathBuf],
        overlays: &[PathBuf],
    ) -> eyre::Result<String> {
        let mut merge_entries: Vec<String> = Vec::new();

        for path in sources.iter().chain(overlays.iter()) {
            let abs = if path.is_absolute() {
                path.clone()
            } else {
                std::env::current_dir()?.join(path)
            };
            merge_entries.push(format!("        - local: {}", abs.display()));
        }

        let yaml = format!(
            r#"variant: fcos
version: "1.5.0"
ignition:
  config:
    merge:
{}
"#,
            merge_entries.join("\n")
        );

        Ok(yaml)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn render_template_basic() {
        let builder = IgnitionBuilder::new("/usr/bin/butane", "/tmp/test")
            .var("k3s_version", "v1.35.1+k3s1")
            .var("arch", "amd64");

        let content = r#"
variant: fcos
version: "1.5.0"
storage:
  files:
    - path: /etc/k3s-version
      contents:
        inline: "{{ k3s_version }}"
"#;

        let rendered = builder.render_template(content, "test").unwrap();
        assert!(rendered.contains("v1.35.1+k3s1"));
        assert!(!rendered.contains("{{ k3s_version }}"));
    }

    #[test]
    fn generate_merge_config_format() {
        let builder = IgnitionBuilder::new("/usr/bin/butane", "/tmp/test");
        let sources = vec![
            PathBuf::from("/tmp/test/source-0.ign"),
            PathBuf::from("/tmp/test/source-1.ign"),
        ];
        let overlays = vec![PathBuf::from("/tmp/test/overlay-0.ign")];

        let config = builder.generate_merge_config(&sources, &overlays).unwrap();
        assert!(config.contains("variant: fcos"));
        assert!(config.contains("source-0.ign"));
        assert!(config.contains("source-1.ign"));
        assert!(config.contains("overlay-0.ign"));
    }
}
