//! Opt-in only: use a small torrent you own or are authorized to share.
use anyhow::{Context, Result};
use serde_json::Value;
use std::time::Duration;
use stremio_torrent::{TorrentConfig, TorrentEngine};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "explicit lawful swarm consent and stream JSON required"]
async fn lawful_swarm_range_smoke() -> Result<()> {
    anyhow::ensure!(
        std::env::var("STREMIO_TORRENT_SMOKE_CONSENT").as_deref() == Ok("I_HAVE_PERMISSION"),
        "set explicit lawful sharing consent"
    );
    let stream: Value = serde_json::from_str(
        &std::env::var("STREMIO_TORRENT_SMOKE_JSON")
            .context("provide a small lawful Stremio torrent stream JSON")?,
    )?;
    let cache = tempfile::tempdir()?;
    let engine = TorrentEngine::with_config(
        cache.path().into(),
        TorrentConfig {
            cache_budget_bytes: 64 * 1024 * 1024, // Admission refuses large torrents; no blind downloads.
            metadata_timeout: Duration::from_secs(60),
            ..Default::default()
        },
    );
    let result = async {
        let prepared = engine.prepare(stream).await?;
        let u = url::Url::parse(prepared["url"].as_str().context("url")?)?;
        let host = format!("{}:{}", u.host_str().unwrap(), u.port().unwrap());
        let mut s = TcpStream::connect(&host).await?;
        s.write_all(
            format!(
                "GET {} HTTP/1.1\r\nHost: {host}\r\nRange: bytes=0-15\r\n\r\n",
                u.path()
            )
            .as_bytes(),
        )
        .await?;
        let mut bytes = Vec::new();
        tokio::time::timeout(Duration::from_secs(60), s.read_to_end(&mut bytes)).await??;
        anyhow::ensure!(
            bytes.starts_with(b"HTTP/1.1 206"),
            "expected partial response"
        );
        let split = bytes
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .context("headers")?;
        anyhow::ensure!(
            bytes.len() - split - 4 == 16,
            "expected sixteen verified bytes"
        );
        Ok::<_, anyhow::Error>(())
    }
    .await;
    engine.stop().await?;
    anyhow::ensure!(engine.stats().await?["active"] == false, "lease must stop");
    result
}
