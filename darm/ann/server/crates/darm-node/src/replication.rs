//! Operator-side replication planning.
//!
//! The swarm never spawns infrastructure itself. Nodes publish `replicate`
//! proposals, validators vote, and this command shows an operator which
//! proposals passed and are still unexecuted. The operator scales with their
//! own credentials (e.g. `kubectl scale`) and then records the proposal as
//! done, so each passed vote is acted on at most once.

use anyhow::{Context, bail};
use std::collections::BTreeSet;
use std::path::Path;

fn load_done(path: &Path) -> anyhow::Result<BTreeSet<String>> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeSet::new()),
        Err(e) => Err(e.into()),
    }
}

pub async fn run(archive: &str, state: &Path, mark_done: Option<&str>) -> anyhow::Result<()> {
    let mut done = load_done(state)?;
    if let Some(id) = mark_done {
        darm_core::Hash32::from_hex(id).context("proposal id must be 64 hex characters")?;
        done.insert(id.to_string());
        let text: String = done.iter().map(|id| format!("{id}\n")).collect();
        std::fs::write(state, text)?;
        println!("recorded {id} as executed");
        return Ok(());
    }
    let url = format!(
        "{}/v1/proposals?kind=replicate&outcome=passed",
        archive.trim_end_matches('/')
    );
    if !(url.starts_with("http://127.0.0.1")
        || url.starts_with("http://localhost")
        || url.starts_with("https://"))
    {
        bail!("refusing plaintext HTTP to a non-loopback archive; use https://");
    }
    let proposals = fetch_json(&url).await?;
    let pending: Vec<_> = proposals
        .as_array()
        .context("archive returned a non-array")?
        .iter()
        .filter(|p| p["id"].as_str().is_some_and(|id| !done.contains(id)))
        .collect();
    if pending.is_empty() {
        println!("no passed, unexecuted replicate proposals");
        return Ok(());
    }
    for proposal in &pending {
        println!(
            "proposal {} by {} passed ({} yes / {} no)",
            proposal["id"].as_str().unwrap_or("?"),
            proposal["author"].as_str().unwrap_or("?"),
            proposal["yes"],
            proposal["no"]
        );
    }
    println!(
        "\nReview, then for each proposal (example for the reference manifests):\n  \
         kubectl -n darm scale statefulset/darm-mesh-node --replicas=<current+1>\n  \
         darm-node replication-plan --archive {archive} --state {} --mark-done <proposal id>",
        state.display()
    );
    Ok(())
}

/// GET via `curl` (no shell; the URL is a single argument) so HTTPS
/// certificate validation uses the system trust store.
async fn fetch_json(url: &str) -> anyhow::Result<serde_json::Value> {
    let output = tokio::process::Command::new("curl")
        .args([
            "--fail",
            "--silent",
            "--show-error",
            "--proto",
            "=https,http",
            "--max-time",
            "30",
            url,
        ])
        .output()
        .await
        .context("running curl")?;
    if !output.status.success() {
        bail!(
            "fetching {url}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}
