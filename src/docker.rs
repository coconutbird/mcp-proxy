//! Per-backend Docker image building and container spawning.
//!
//! When a backend has an `install` config and the binary isn't available
//! locally, we build a small Docker image for it and run the backend
//! inside a container with stdio piped.

use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use tokio::process::Command;

use crate::config::{ExtractMethod, InstallConfig, ServerConfig, expand_env_with_overrides};

/// Docker image tag for a backend: `mcp-proxy/<name>`
fn image_tag(name: &str) -> String {
    format!("mcp-proxy/{name}")
}

/// Check if a Docker image already exists locally.
async fn image_exists(tag: &str) -> bool {
    Command::new("docker")
        .args(["image", "inspect", tag])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .is_ok_and(|s| s.success())
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

/// Build the Docker image for a backend if it doesn't already exist.
pub async fn ensure_image(name: &str, cfg: &ServerConfig) -> Result<()> {
    let tag = image_tag(name);

    if image_exists(&tag).await {
        return Ok(());
    }

    let dockerfile =
        generate_dockerfile(cfg).ok_or_else(|| anyhow::anyhow!("no install config for {name}"))?;

    eprintln!("  building docker image {tag}...");

    let mut child = Command::new("docker")
        .args(["build", "-t", &tag, "-f", "-", "."])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("failed to run 'docker build' — is Docker installed?")?;

    // Feed the Dockerfile via stdin
    use tokio::io::AsyncWriteExt;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(dockerfile.as_bytes()).await?;
        // stdin drops here, closing it
    }

    let output = child.wait_with_output().await?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("docker build failed for {name}:\n{stderr}");
    }

    eprintln!("  built {tag}");
    Ok(())
}

/// Spawn a Docker container for a backend, returning the `Child` process.
///
/// Runs `docker run -i --rm` so stdio is piped and the container is removed
/// on exit. Environment variables are passed via `-e` flags.
pub async fn run_container(
    name: &str,
    cfg: &ServerConfig,
    extra_env: &HashMap<String, String>,
) -> Result<tokio::process::Child> {
    let tag = image_tag(name);
    let expand = |s: &str| expand_env_with_overrides(s, extra_env);

    let mut cmd = Command::new("docker");
    cmd.args(["run", "-i", "--rm", "--name", &format!("mcp-proxy-{name}")]);

    // Pass environment variables
    let mut merged_env: HashMap<String, String> = cfg
        .env
        .iter()
        .map(|(k, v)| (k.clone(), expand(v)))
        .collect();
    for (k, v) in extra_env {
        merged_env.entry(k.clone()).or_insert_with(|| v.clone());
    }
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

/// Check whether a command exists on the local system.
pub fn command_exists(cmd: &str) -> bool {
    which::which(cmd).is_ok()
}
