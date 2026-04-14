//! Per-backend Docker image building and container spawning.
//!
//! When a backend has an `install` config and the binary isn't available
//! locally, we build a small Docker image for it and run the backend
//! inside a container with stdio piped.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use tracing::{debug, info};

use anyhow::{Context, Result, bail};
use tokio::process::Command;

use crate::config::{
    ExtractMethod, InstallConfig, ServerConfig, expand_env_with_overrides, resolve_env,
};
use crate::util::fnv1a;

/// Monotonic counter for unique container name suffixes.
static CONTAINER_SEQ: AtomicU64 = AtomicU64::new(0);

/// Return a unique u32 suffix for container names, based on process ID and a
/// monotonic counter. Collision-free within the same process.
fn unique_suffix() -> u32 {
    let seq = CONTAINER_SEQ.fetch_add(1, Ordering::Relaxed);
    fnv1a(&format!("{}{seq}", std::process::id())) as u32
}

/// Docker image tag for a backend: `mcp-proxy/<name>`
fn image_tag(name: &str) -> String {
    format!("mcp-proxy/{name}")
}

/// Generate a Dockerfile for a single backend.
fn generate_dockerfile(cfg: &ServerConfig) -> Option<String> {
    let install = cfg.install.as_ref()?;

    let mut d = String::from("FROM node:lts-slim\n\n");

    // Base deps
    let needs_python = matches!(install, InstallConfig::Pip { .. });
    let needs_curl = matches!(install, InstallConfig::Binary { .. });
    let needs_unzip = matches!(
        install,
        InstallConfig::Binary {
            extract: Some(ExtractMethod::Zip),
            ..
        }
    );

    let mut apt = vec!["ca-certificates"];
    if needs_curl {
        apt.push("curl");
    }
    if needs_python {
        apt.push("python3");
        apt.push("python3-pip");
    }
    if needs_unzip {
        apt.push("unzip");
    }

    d.push_str(&format!(
        "RUN apt-get update && apt-get install -y --no-install-recommends \\\n    {} \\\n && rm -rf /var/lib/apt/lists/*\n",
        apt.join(" \\\n    ")
    ));

    // Install the backend
    match install {
        InstallConfig::Npm { package } => {
            d.push_str(&format!("\nRUN npm install -g {package}\n"));
        }
        InstallConfig::Pip { package } => {
            d.push_str(&format!(
                "\nRUN pip3 install --break-system-packages {package}\n"
            ));
        }
        InstallConfig::Binary {
            url,
            extract,
            binary,
        } => {
            let has_arch = url.contains("${ARCH}");
            if has_arch {
                d.push_str(&format!(
                    "\nARG TARGETARCH\nRUN ARCH=$(case \"${{TARGETARCH}}\" in \\\n      \"amd64\") echo \"x86_64\" ;; \\\n      \"arm64\") echo \"arm64\" ;; \\\n      *) echo \"x86_64\" ;; \\\n    esac) && \\\n    curl -fsSL \"{url}\" -o /tmp/dl"
                ));
            } else {
                d.push_str(&format!("\nRUN curl -fsSL \"{url}\" -o /tmp/dl"));
            }
            match extract.as_ref().unwrap_or(&ExtractMethod::None) {
                ExtractMethod::Tar => d.push_str(&format!(
                    " && \\\n    tar -xzf /tmp/dl -C /usr/local/bin {binary} && \\\n    rm /tmp/dl && chmod +x /usr/local/bin/{binary}\n"
                )),
                ExtractMethod::Zip => d.push_str(&format!(
                    " && \\\n    unzip /tmp/dl -d /tmp/ex && \\\n    mv /tmp/ex/{binary} /usr/local/bin/ && \\\n    rm -rf /tmp/dl /tmp/ex && chmod +x /usr/local/bin/{binary}\n"
                )),
                ExtractMethod::None => d.push_str(&format!(
                    " && \\\n    mv /tmp/dl /usr/local/bin/{binary} && chmod +x /usr/local/bin/{binary}\n"
                )),
            }
        }
        InstallConfig::Npx => {
            // npx is already available in node:lts-slim
        }
    }

    d.push_str(&format!("\nENTRYPOINT [\"{}\"]\n", cfg.command));
    Some(d)
}

const HASH_LABEL: &str = "mcp-proxy.config-hash";

/// Hash the Dockerfile content to detect config changes.
/// Uses FNV-1a for determinism across Rust compiler versions.
fn content_hash(dockerfile: &str) -> String {
    format!("{:016x}", fnv1a(dockerfile))
}

/// Read the config-hash label from an existing image (if any).
async fn image_hash(tag: &str) -> Option<String> {
    let output = Command::new("docker")
        .args([
            "inspect",
            "--format",
            &format!("{{{{index .Config.Labels \"{HASH_LABEL}\"}}}}"),
            tag,
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

/// Build the Docker image for a backend. Rebuilds if the config has changed.
pub async fn ensure_image(name: &str, cfg: &ServerConfig) -> Result<()> {
    let tag = image_tag(name);

    let dockerfile =
        generate_dockerfile(cfg).ok_or_else(|| anyhow::anyhow!("no install config for {name}"))?;

    let hash = content_hash(&dockerfile);

    // Skip build if image exists with matching hash
    if let Some(existing) = image_hash(&tag).await {
        if existing == hash {
            return Ok(());
        }
        info!(server = name, "config changed, rebuilding docker image");
    }

    info!(server = name, tag = %tag, "building docker image");

    // Inject the hash as a label so we can detect changes next time
    let label_arg = format!("LABEL {HASH_LABEL}=\"{hash}\"");
    let dockerfile_with_label = format!("{dockerfile}\n{label_arg}\n");

    let mut child = Command::new("docker")
        .args(["build", "-t", &tag, "-f", "-", "."])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to run 'docker build' — is Docker installed?")?;

    use tokio::io::AsyncWriteExt;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(dockerfile_with_label.as_bytes()).await?;
    }

    let output = child.wait_with_output().await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("docker build failed for {name}:\n{stderr}");
    }

    debug!(server = name, tag = %tag, "docker image built");
    Ok(())
}

/// Spawn a Docker container for a backend, returning the `Child` process.
///
/// Runs `docker run -i --rm` so stdio is piped and the container is removed
/// on exit. Environment variables are passed via `-e` flags.
/// Container names include a random suffix to avoid collisions when multiple
/// instances of the same server run with different credentials.
pub async fn run_container(
    name: &str,
    cfg: &ServerConfig,
    extra_env: &HashMap<String, String>,
) -> Result<tokio::process::Child> {
    let tag = image_tag(name);
    let expand = |s: &str| expand_env_with_overrides(s, extra_env);

    // Include a short random suffix to avoid naming collisions when
    // multiple credential-scoped instances of the same server run.
    let suffix: u32 = unique_suffix();
    let container_name = format!("mcp-proxy-{name}-{suffix:08x}");

    let mut cmd = Command::new("docker");
    cmd.args(["run", "-i", "--rm", "--name", &container_name]);

    // Pass environment variables
    let merged_env = resolve_env(&cfg.env, extra_env);
    for (k, v) in &merged_env {
        cmd.args(["-e", &format!("{k}={v}")]);
    }

    // Image + command args
    cmd.arg(&tag);
    for a in &cfg.args {
        cmd.arg(expand(a));
    }

    let child = cmd
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context(format!("failed to run container for {name}"))?;

    Ok(child)
}
