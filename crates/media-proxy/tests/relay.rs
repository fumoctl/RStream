use rustmedia_media_proxy::MediaProxy;
use serde_json::json;
use std::sync::{Arc, Mutex};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};

async fn start_mock<F>(handler: F) -> (String, Arc<Mutex<Vec<String>>>)
where
    F: Fn(&str, &str) -> String + Send + Sync + 'static,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let authority = listener.local_addr().unwrap().to_string();
    let authority_for_task = authority.clone();
    let log = Arc::new(Mutex::new(Vec::new()));
    let log_clone = log.clone();
    let handler = Arc::new(handler);

    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let log = log_clone.clone();
            let handler = handler.clone();
            let auth = authority_for_task.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.len() < 8192 {
                    let Ok(n) = sock.read(&mut chunk).await else {
                        return;
                    };
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let req = String::from_utf8_lossy(&buf).into_owned();
                log.lock().unwrap().push(req.clone());
                let resp = handler(&req, &auth);
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.shutdown().await;
            });
        }
    });

    (authority, log)
}

#[tokio::test]
async fn probe_preserves_headers_same_origin_and_strips_cross_origin() {
    let (cdn_auth, cdn_log) = start_mock(|_req, _auth| {
        "HTTP/1.1 206 Partial Content\r\nContent-Type: video/x-matroska\r\nContent-Range: bytes 0-0/104857600\r\nContent-Length: 1\r\nConnection: close\r\n\r\nX".to_string()
    })
    .await;

    let cdn_target = format!("http://{cdn_auth}/video.mkv");
    let (origin_auth, origin_log) = start_mock(move |req, auth| {
        if req.starts_with("GET /same-hop ") {
            format!("HTTP/1.1 302 Found\r\nLocation: http://{auth}/cross-hop\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        } else {
            format!("HTTP/1.1 302 Found\r\nLocation: {cdn_target}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        }
    })
    .await;

    let proxy = MediaProxy::default();
    let headers = json!({
        "Authorization": "Bearer top-secret-token",
        "X-Custom": "origin-only"
    });
    let res = proxy
        .probe(&format!("http://{origin_auth}/same-hop"), &headers, true)
        .await
        .unwrap();

    assert_eq!(res["ok"], true);
    assert_eq!(res["status"], 206);
    assert_eq!(res["bytes"], 104_857_600);
    assert_eq!(res["playlist"], false);

    let origin_reqs = origin_log.lock().unwrap();
    assert_eq!(origin_reqs.len(), 2, "expected 2 requests to initial origin");
    for r in origin_reqs.iter() {
        let lower = r.to_ascii_lowercase();
        assert!(lower.contains("authorization: bearer top-secret-token"));
        assert!(lower.contains("x-custom: origin-only"));
        assert!(lower.contains("range: bytes=0-0"));
    }

    let cdn_reqs = cdn_log.lock().unwrap();
    assert_eq!(cdn_reqs.len(), 1);
    let cdn_lower = cdn_reqs[0].to_ascii_lowercase();
    assert!(
        !cdn_lower.contains("authorization:") && !cdn_lower.contains("x-custom:"),
        "cross-origin redirect must strip custom headers: {}",
        cdn_reqs[0]
    );
    assert!(cdn_lower.contains("range: bytes=0-0"));
}

#[tokio::test]
async fn probe_rejects_loopback_without_allow_http_and_unspecified_redirects() {
    let (auth, log) = start_mock(|req, _auth| {
        if req.starts_with("GET /to-zero ") {
            "HTTP/1.1 302 Found\r\nLocation: http://0.0.0.0:9/evil\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string()
        } else {
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-0/104857600\r\nContent-Length: 1\r\nConnection: close\r\n\r\nX".to_string()
        }
    })
    .await;

    let proxy = MediaProxy::new();

    // Production mode (`allow_http = false`) forbids probing loopback hosts entirely.
    for url in [
        format!("http://{auth}/movie.mkv"),
        "http://127.0.0.1:9/movie.mkv".to_string(),
        "https://127.0.0.1:9/movie.mkv".to_string(),
        "http://[::1]:9/movie.mkv".to_string(),
        "http://[::ffff:127.0.0.1]:9/movie.mkv".to_string(),
        "http://localhost:9/movie.mkv".to_string(),
        "http://0.0.0.0:9/movie.mkv".to_string(),
        "http://[::]:9/movie.mkv".to_string(),
    ] {
        assert!(
            proxy.probe(&url, &json!({}), false).await.is_err(),
            "must reject {url} when allow_http=false"
        );
    }
    assert!(
        log.lock().unwrap().is_empty(),
        "rejected loopback probe must never connect to local listener"
    );

    // Redirect to unspecified address (`0.0.0.0`) is rejected even with allow_http = true.
    assert!(proxy
        .probe(&format!("http://{auth}/to-zero"), &json!({}), true)
        .await
        .is_err());
}

#[tokio::test]
async fn probe_detects_range_content_length_playlists_and_provider_status_clips() {
    let (auth, _log) = start_mock(|req, auth| {
        if req.starts_with("GET /full200 ") {
            "HTTP/1.1 200 OK\r\nContent-Type: video/mp4\r\nContent-Length: 83886080\r\nConnection: close\r\n\r\nX".to_string()
        } else if req.starts_with("GET /tiny ") {
            "HTTP/1.1 206 Partial Content\r\nContent-Type: video/mp4\r\nContent-Range: bytes 0-0/1048576\r\nContent-Length: 1\r\nConnection: close\r\n\r\nX".to_string()
        } else if req.starts_with("GET /live.m3u8 ") {
            "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.apple.mpegurl\r\nContent-Length: 512\r\nConnection: close\r\n\r\n#".to_string()
        } else if req.starts_with("GET /provider-status ") {
            format!("HTTP/1.1 302 Found\r\nLocation: http://{auth}/videos/downloading_v2.mp4\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        } else {
            "HTTP/1.1 200 OK\r\nContent-Type: video/mp4\r\nContent-Length: 524288\r\nConnection: close\r\n\r\nX".to_string()
        }
    })
    .await;

    let proxy = MediaProxy::new();

    let full = proxy
        .probe(&format!("http://{auth}/full200"), &json!(null), true)
        .await
        .unwrap();
    assert_eq!(full["ok"], true);
    assert_eq!(full["status"], 200);
    assert_eq!(full["bytes"], 83_886_080);

    let tiny = proxy
        .probe(&format!("http://{auth}/tiny"), &json!(null), true)
        .await
        .unwrap();
    assert_eq!(tiny["ok"], false);
    assert_eq!(tiny["bytes"], 1_048_576);

    let hls = proxy
        .probe(&format!("http://{auth}/live.m3u8"), &json!(null), true)
        .await
        .unwrap();
    assert_eq!(hls["ok"], true);
    assert_eq!(hls["playlist"], true);

    let provider_clip = proxy
        .probe(&format!("http://{auth}/provider-status"), &json!(null), true)
        .await
        .unwrap();
    assert_eq!(provider_clip["ok"], false);
    assert_eq!(provider_clip["provider_status"], "downloading");
}
