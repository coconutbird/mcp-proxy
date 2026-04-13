use crate::config::{Config, ExtractMethod, InstallConfig};
use anyhow::Result;
use std::path::Path;

/// Generate a Dockerfile from the config.
pub fn dockerfile(config: &Config, out: &Path) -> Result<()> {
    let mut npm: Vec<String> = vec![];
    let mut pip: Vec<String> = vec![];
    let mut bins: Vec<(String, String, Option<ExtractMethod>, String)> = vec![]; // name,url,extract,binary
    let mut need_zip = false;

    for (name, srv) in &config.servers {
        if name.starts_with('_') {
            continue;
        }
        match &srv.install {
            Some(InstallConfig::Npm { package }) => npm.push(package.clone()),
            Some(InstallConfig::Pip { package }) => pip.push(package.clone()),
            Some(InstallConfig::Binary {
                url,
                extract,
                binary,
            }) => {
                if matches!(extract, Some(ExtractMethod::Zip)) {
                    need_zip = true;
                }
                bins.push((name.clone(), url.clone(), extract.clone(), binary.clone()));
            }
            Some(InstallConfig::Npx) | None => {}
        }
    }

    let mut d = String::from("FROM node:lts-slim\n\n");
    d.push_str("RUN apt-get update && apt-get install -y --no-install-recommends \\\n    curl \\\n    ca-certificates");
    if !pip.is_empty() {
        d.push_str(" \\\n    python3 \\\n    python3-pip");
    }
    if need_zip {
        d.push_str(" \\\n    unzip");
    }
    d.push_str(" && \\\n    rm -rf /var/lib/apt/lists/*\n");

    for (name, url, extract, binary) in &bins {
        let has_arch = url.contains("${ARCH}");
        if has_arch {
            d.push_str(&format!("\nARG TARGETARCH\nRUN ARCH=$(case \"${{TARGETARCH}}\" in \\\n      \"amd64\") echo \"x86_64\" ;; \\\n      \"arm64\") echo \"arm64\" ;; \\\n      *) echo \"x86_64\" ;; \\\n    esac) && \\\n    curl -fsSL \"{url}\" -o /tmp/{name}-dl"));
        } else {
            d.push_str(&format!("\nRUN curl -fsSL \"{url}\" -o /tmp/{name}-dl"));
        }
        match extract.as_ref().unwrap_or(&ExtractMethod::None) {
            ExtractMethod::Tar => d.push_str(&format!(" && \\\n    tar -xzf /tmp/{name}-dl -C /usr/local/bin {binary} && \\\n    rm /tmp/{name}-dl && chmod +x /usr/local/bin/{binary}\n")),
            ExtractMethod::Zip => d.push_str(&format!(" && \\\n    unzip /tmp/{name}-dl -d /tmp/{name}-ex && \\\n    mv /tmp/{name}-ex/{binary} /usr/local/bin/ && \\\n    rm -rf /tmp/{name}-dl /tmp/{name}-ex && chmod +x /usr/local/bin/{binary}\n")),
            ExtractMethod::None => d.push_str(&format!(" && \\\n    mv /tmp/{name}-dl /usr/local/bin/{binary} && chmod +x /usr/local/bin/{binary}\n")),
        }
    }

    if !npm.is_empty() {
        d.push_str(&format!(
            "\nRUN npm install -g \\\n    {}\n",
            npm.join(" \\\n    ")
        ));
    }
    if !pip.is_empty() {
        d.push_str(&format!(
            "\nRUN pip3 install --break-system-packages \\\n    {}\n",
            pip.join(" \\\n    ")
        ));
    }

    d.push_str("\nWORKDIR /app\nCOPY config/servers.json /app/config/servers.json\nCOPY mcp-proxy /usr/local/bin/mcp-proxy\n\nENV CONFIG_PATH=/app/config/servers.json\nENV MCP_TRANSPORT=http\nENV MCP_PORT=3000\n\nEXPOSE 3000\nCMD [\"mcp-proxy\", \"serve\"]\n");

    std::fs::write(out, d)?;
    eprintln!("wrote {}", out.display());
    Ok(())
}

/// Generate .env.example from config.
pub fn env_example(config: &Config, out: &Path) -> Result<()> {
    let mut toggles = vec![];
    let mut vars: Vec<(String, String)> = vec![];

    for (name, srv) in &config.servers {
        if name.starts_with('_') {
            continue;
        }
        if let Some(t) = &srv.env_toggle {
            toggles.push(t.clone());
        }
        for val in srv.env.values().chain(srv.args.iter()) {
            let mut rest = val.as_str();
            while let Some(i) = rest.find("${") {
                if let Some(j) = rest[i..].find('}') {
                    let var = &rest[i + 2..i + j];
                    if !vars.iter().any(|(n, _)| n == var) {
                        vars.push((var.into(), name.clone()));
                    }
                    rest = &rest[i + j + 1..];
                } else {
                    break;
                }
            }
        }
    }

    let mut s = String::from(
        "# MCP Proxy — Environment Configuration\n# Auto-generated from config/servers.json\n\nMCP_PORT=3000\n\n# Enable/disable servers (all enabled by default, set false to disable)\n",
    );
    for t in &toggles {
        s.push_str(&format!("{t}=true\n"));
    }
    s.push_str("\n# API tokens / credentials\n");
    let mut by_srv: std::collections::HashMap<String, Vec<String>> = Default::default();
    for (var, srv) in &vars {
        by_srv.entry(srv.clone()).or_default().push(var.clone());
    }
    for (srv, vs) in &by_srv {
        s.push_str(&format!("\n# {srv}\n"));
        for v in vs {
            s.push_str(&format!("{v}=\n"));
        }
    }

    std::fs::write(out, s)?;
    eprintln!("wrote {}", out.display());
    Ok(())
}
