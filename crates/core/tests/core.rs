mod common;
use common::Data;
use rustmedia_core::Core;
use serde_json::{json, Value};
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread,
};
struct Reply {
    status: u16,
    body: Value,
    cache: &'static str,
    location: Option<String>,
}
impl Reply {
    fn ok(body: Value) -> Self {
        Self {
            status: 200,
            body,
            cache: "max-age=3600",
            location: None,
        }
    }
}
struct Server {
    addr: std::net::SocketAddr,
    paths: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    join: Option<thread::JoinHandle<()>>,
}
impl Server {
    fn new(handler: impl Fn(&str) -> Reply + Send + Sync + 'static) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let paths = Arc::new(Mutex::new(Vec::new()));
        let (s, p, h) = (stop.clone(), paths.clone(), Arc::new(handler));
        let join = thread::spawn(move || {
            let mut workers = Vec::new();
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                if s.load(Ordering::SeqCst) {
                    break;
                }
                let (p, h) = (p.clone(), h.clone());
                workers.push(thread::spawn(move || {
                    stream.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
                    let mut all = Vec::new();
                    let mut buf = [0; 2048];
                    loop {
                        match stream.read(&mut buf) {
                            Ok(0) | Err(_) => return,
                            Ok(n) => all.extend_from_slice(&buf[..n]),
                        }
                        if all.windows(4).any(|w| w == b"\r\n\r\n") { break; }
                        if all.len() > 32768 { return; }
                    }
                    let req = String::from_utf8_lossy(&all);
                    let path = req.lines().next().unwrap().split_whitespace().nth(1).unwrap().to_owned();
                    p.lock().unwrap().push(path.clone());
                    let reply = h(&path);
                    let body = reply.body.to_string();
                    let location = reply.location.map(|s| format!("Location: {s}\r\n")).unwrap_or_default();
                    let response = format!(
                        "HTTP/1.1 {} OK\r\nContent-Type: application/json\r\nCache-Control: {}\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                        reply.status, reply.cache, location, body.len(), body
                    );
                    let _ = stream.write_all(response.as_bytes());
                }));
            }
            for worker in workers {
                worker.join().unwrap();
            }
        });
        Self {
            addr,
            paths,
            stop,
            join: Some(join),
        }
    }
    fn url(&self) -> String {
        format!("http://{}/configured/manifest.json?key=demo", self.addr)
    }
    fn count(&self, part: &str) -> usize {
        self.paths
            .lock()
            .unwrap()
            .iter()
            .filter(|p| p.contains(part))
            .count()
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr);
        self.join.take().unwrap().join().unwrap();
    }
}
fn manifest() -> Value {
    json!({
        "id":"test.local","name":"Local test","version":"1.0.0","types":["movie","series","anime"],
        "resources":["catalog",{"name":"meta","types":["movie","series","anime"],"idPrefixes":["tt","kitsu:"]},{"name":"stream","types":["series"],"idPrefixes":["tt"]},"subtitles"],
        "catalogs":[{"type":"movie","id":"top","extra":[{"name":"search"},{"name":"skip"}]},{"type":"anime","id":"required","extra":[{"name":"search"},{"name":"genre","isRequired":true}]}],"unknown":{"preserve":true}
    })
}
fn db_count(d: &Data, table: &str) -> i64 {
    d.db()
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}
async fn empty(core: &Core) {
    let a = core.execute(json!({"op":"addons"})).await.unwrap();
    for a in a["addons"].as_array().unwrap() {
        core.execute(json!({"op":"remove_addon","url":a["manifest_url"]}))
            .await
            .unwrap();
    }
}
async fn install(core: &Core, s: &Server) {
    core.execute(json!({"op":"install_addon","url":s.url()}))
        .await
        .unwrap();
}

#[tokio::test]
async fn startup_persistence_and_send_sync() {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<Core>();
    let d = Data::new();
    let core = d.core();
    assert_eq!(
        core.execute(json!({"op":"addons"})).await.unwrap()["addons"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
    empty(&core).await;
    drop(core);
    assert_eq!(
        d.core().execute(json!({"op":"addons"})).await.unwrap()["addons"],
        json!([])
    );
}
#[tokio::test]
async fn paths_extras_cache_and_required_search() {
    let s = Server::new(|p| {
        if p.contains("manifest.json") {
            Reply::ok(manifest())
        } else {
            Reply::ok(json!({"metas":[{"id":"tt1","type":"movie","name":"Raw name"}],"custom":42}))
        }
    });
    let d = Data::new();
    let core = d.core();
    empty(&core).await;
    install(&core, &s).await;
    let req = json!({"op":"catalog","addon_url":s.url(),"type":"movie","id":"top","extra":{"search":"a/b & 日本","skip":10}});
    let a = core.execute(req.clone()).await.unwrap();
    let b = core.execute(req).await.unwrap();
    assert_eq!(a["cached"], false);
    assert_eq!(b["cached"], true);
    assert_eq!(b["custom"], 42);
    assert_eq!(s.count("/catalog/"), 1);
    assert!(s.paths.lock().unwrap().contains(&"/configured/catalog/movie/top/search=a%2Fb%20%26%20%E6%97%A5%E6%9C%AC&skip=10.json?key=demo".to_owned()));
    let result = core
        .execute(json!({"op":"search","query":"another"}))
        .await
        .unwrap();
    assert_eq!(result["groups"].as_array().unwrap().len(), 1);
    assert_eq!(result["groups"][0]["catalog_id"], "top");
    assert!(core
        .execute(json!({"op":"catalog","addon_url":s.url(),"type":"anime","id":"required"}))
        .await
        .is_err());
    assert_eq!(s.count("/anime/required/"), 0);
    assert_eq!(
        core.execute(json!({"op":"search","query":"anime","extra":{"genre":"Adventure"}}))
            .await
            .unwrap()["groups"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}
#[tokio::test]
async fn search_work_is_bounded_for_hostile_manifests() {
    let catalogs = (0..200)
        .map(|i| json!({"type":"movie","id":format!("c{i}"),"extra":[{"name":"search"}]}))
        .collect::<Vec<_>>();
    let hostile = json!({"id":"hostile.local","name":"Hostile","version":"1.0.0",
        "types":["movie"],"resources":["catalog"],"catalogs":catalogs});
    let s = Server::new(move |p| {
        if p.contains("manifest.json") {
            Reply::ok(hostile.clone())
        } else {
            Reply::ok(json!({"metas":[]}))
        }
    });
    let d = Data::new();
    let core = d.core();
    empty(&core).await;
    install(&core, &s).await;
    let result = core
        .execute(json!({"op":"search","query":"x"}))
        .await
        .unwrap();
    assert_eq!(result["groups"].as_array().unwrap().len(), 32);
    assert!(
        s.count("/catalog/") <= 32,
        "search queued {} catalog requests",
        s.count("/catalog/")
    );
}

#[tokio::test]
async fn durable_metadata_is_bounded_across_oversized_merges() {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let s = Server::new(move |p| {
        if p.contains("manifest.json") {
            Reply::ok(manifest())
        } else if p.contains("/meta/") {
            let n = counter.fetch_add(1, Ordering::SeqCst);
            let mut body = json!({"meta":{"id":"tt1","type":"movie","name":"Bounded"}});
            body["meta"][format!("bulk{n}")] = json!("x".repeat(300 * 1024));
            Reply {
                status: 200,
                body,
                cache: "max-age=0",
                location: None,
            }
        } else {
            Reply::ok(json!({"metas":[]}))
        }
    });
    let d = Data::new();
    let core = d.core();
    empty(&core).await;
    install(&core, &s).await;
    core.execute(json!({"op":"set_library","type":"movie","id":"tt1",
        "addon_url":s.url(),"saved":true,"meta":{"id":"tt1","name":"Bounded"}}))
        .await
        .unwrap();
    for _ in 0..8 {
        core.execute(
            json!({"op":"meta","addon_url":s.url(),"type":"movie","id":"tt1","force":true}),
        )
        .await
        .unwrap();
    }
    assert_eq!(calls.load(Ordering::SeqCst), 8);
    let library = core.execute(json!({"op":"library"})).await.unwrap();
    let item = library["items"]
        .as_array()
        .unwrap()
        .first()
        .unwrap()
        .to_string();
    assert!(
        item.len() < 1_200_000,
        "durable metadata grew to {} bytes",
        item.len()
    );
}

#[tokio::test]
async fn streams_are_verbatim_filtered_and_never_persisted() {
    let raw = json!([{"name":"Provider\n1080p 🟢","title":"Title\r\nSecond line","description":"Untouched  text\nsize: 1 GB","url":"https://example.test/video?secret=do-not-persist","behaviorHints":{"bingeGroup":"raw"},"unknown":[1,{"x":true}]},{"infoHash":"0000000000000000000000000000000000000000","fileIdx":3}]);
    let returned = raw.clone();
    let s = Server::new(move |p| {
        if p.contains("manifest.json") {
            Reply::ok(manifest())
        } else {
            Reply::ok(json!({"streams":returned}))
        }
    });
    let d = Data::new();
    let core = d.core();
    empty(&core).await;
    install(&core, &s).await;
    let req = json!({"op":"streams","type":"series","id":"tt123:1/2"});
    assert_eq!(
        core.execute(req.clone()).await.unwrap()["groups"][0]["streams"],
        raw
    );
    assert_eq!(
        core.execute(req.clone()).await.unwrap()["groups"][0]["streams"],
        raw
    );
    assert_eq!(s.count("/stream/"), 1);
    assert!(s
        .paths
        .lock()
        .unwrap()
        .contains(&"/configured/stream/series/tt123%3A1%2F2.json?key=demo".to_owned()));
    for (kind, id) in [
        ("anime", "kitsu:123"),
        ("movie", "tt123"),
        ("series", "other:1"),
    ] {
        assert_eq!(
            core.execute(json!({"op":"streams","type":kind,"id":id}))
                .await
                .unwrap()["groups"],
            json!([])
        );
    }
    assert_eq!(db_count(&d, "cache"), 0);
    drop(core);
    let core = d.core();
    core.execute(req).await.unwrap();
    assert_eq!(s.count("/stream/"), 2);
    for name in ["core.sqlite3", "core.sqlite3-wal"] {
        if let Ok(bytes) = std::fs::read(d.0.join(name)) {
            assert!(!bytes
                .windows(b"do-not-persist".len())
                .any(|w| w == b"do-not-persist"));
        }
    }
}
#[tokio::test]
async fn stale_force_no_store_and_missing_fields() {
    let failing = Arc::new(AtomicBool::new(false));
    let flag = failing.clone();
    let s = Server::new(move |p| {
        if p.contains("manifest.json") {
            Reply::ok(manifest())
        } else if flag.load(Ordering::SeqCst) {
            Reply {
                status: 503,
                body: json!({}),
                cache: "no-store",
                location: None,
            }
        } else {
            Reply::ok(json!({}))
        }
    });
    let d = Data::new();
    let core = d.core();
    empty(&core).await;
    install(&core, &s).await;
    let mut req = json!({"op":"catalog","addon_url":s.url(),"type":"movie","id":"top"});
    assert_eq!(core.execute(req.clone()).await.unwrap()["metas"], json!([]));
    assert!(core
        .execute(json!({"op":"meta","addon_url":s.url(),"type":"movie","id":"tt1"}))
        .await
        .is_err());
    failing.store(true, Ordering::SeqCst);
    req["force"] = json!(true);
    let stale = core.execute(req).await.unwrap();
    assert_eq!(stale["stale"], true);
    assert_eq!(stale["cached"], true);
    assert!(!stale["error"].as_str().unwrap().contains("key=demo"));
    let no_store = Server::new(|p| {
        if p.contains("manifest.json") {
            Reply::ok(manifest())
        } else {
            Reply {
                status: 200,
                body: json!({"metas":[]}),
                cache: "no-store",
                location: None,
            }
        }
    });
    install(&core, &no_store).await;
    for _ in 0..2 {
        assert_eq!(
            core.execute(
                json!({"op":"catalog","addon_url":no_store.url(),"type":"movie","id":"top"})
            )
            .await
            .unwrap()["cached"],
            false
        );
    }
    assert_eq!(no_store.count("/catalog/"), 2);
}
#[tokio::test]
async fn http_consent_redirect_validation_and_bad_requests() {
    let d = Data::new();
    let core = d.core();
    empty(&core).await;
    let e = core
        .execute(json!({"op":"install_addon","url":"http://example.invalid/private/manifest.json?token=secret"}))
        .await
        .unwrap_err();
    assert!(e.to_string().contains("allow_http"));
    assert!(!e.to_string().contains("secret"));
    for req in [
        json!(null),
        json!({}),
        json!({"op":"bogus"}),
        json!({"op":"progress"}),
        json!({"op":"set_library","type":"movie","id":"tt1","addon_url":"https://example.test/manifest.json","saved":"true"}),
    ] {
        assert!(core.execute(req).await.is_err());
    }
    let bad = Server::new(|_| Reply::ok(json!({"id":"bad","name":"Missing version"})));
    assert!(core
        .execute(json!({"op":"install_addon","url":bad.url()}))
        .await
        .is_err());
    let redirect = Server::new(|_| Reply {
        status: 302,
        body: json!({}),
        cache: "no-store",
        location: Some("http://example.invalid/manifest.json".into()),
    });
    assert!(core
        .execute(json!({"op":"install_addon","url":redirect.url()}))
        .await
        .unwrap_err()
        .to_string()
        .contains("allow_http"));
}
#[tokio::test]
async fn history_seek_is_not_watching_and_identities_do_not_collide() {
    let d = Data::new();
    let core = d.core();
    let a = "https://a.test/config/manifest.json";
    let b = "https://b.test/manifest.json";
    let item = core
        .execute(json!({"op":"progress","type":"movie","id":"tt1","addon_url":a,"position_ms":99000,"duration_ms":100000,"meta":{"name":"A"}}))
        .await
        .unwrap();
    assert_eq!(item["item"]["watched"], false);
    assert_eq!(item["item"]["watched_ms"], 0);
    core.execute(json!({"op":"progress","type":"movie","id":"tt1","addon_url":a,"position_ms":99000,"duration_ms":100000,"watched_ms":85000})).await.unwrap();
    for (kind, id, addon) in [
        ("movie", "tt1", b),
        ("series", "tt1", a),
        ("anime", "kitsu:1", a),
        ("anime", "kitsu:1", b),
        ("anime", "tt1", a),
        ("movie", "custom:1", a),
        ("movie", "custom:1", b),
    ] {
        core.execute(
            json!({"op":"set_library","type":kind,"id":id,"addon_url":addon,"saved":true}),
        )
        .await
        .unwrap();
    }
    let lib = core.execute(json!({"op":"library"})).await.unwrap();
    assert_eq!(lib["items"].as_array().unwrap().len(), 6);
    let watched = lib["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "tt1" && m["type"] == "movie")
        .unwrap();
    assert_eq!(watched["watched"], true);
    assert_eq!(watched["watched_ms"], 85000);
    drop(core);
    assert_eq!(
        d.core().execute(json!({"op":"library"})).await.unwrap()["items"]
            .as_array()
            .unwrap()
            .len(),
        6
    );
}
#[tokio::test]
async fn subtitles_preserve_unknown_fields_and_encode_extras() {
    let s = Server::new(|p| {
        if p.contains("manifest.json") {
            Reply::ok(manifest())
        } else {
            Reply::ok(
                json!({"subtitles":[{"id":"s1","lang":"eng","url":"https://example.test/s.vtt","unknown":"raw"}]}),
            )
        }
    });
    let d = Data::new();
    let core = d.core();
    empty(&core).await;
    install(&core, &s).await;
    let subtitles = core
        .execute(json!({"op":"subtitles","type":"movie","id":"tt1","extra":{"videoHash":"a/b"}}))
        .await
        .unwrap();
    assert_eq!(subtitles["subtitles"][0]["unknown"], "raw");
    assert!(s
        .paths
        .lock()
        .unwrap()
        .iter()
        .any(|p| p.contains("/subtitles/movie/tt1/videoHash=a%2Fb.json")));
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_calls_are_send_and_database_is_not_locked_during_network() {
    let d = Data::new();
    let core = d.core();
    let mut jobs = Vec::new();
    for n in 0..24 {
        let c = core.clone();
        jobs.push(tokio::spawn(async move{c.execute(json!({"op":"set_library","type":"anime","id":format!("kitsu:{n}"),"addon_url":"https://example.test/manifest.json","saved":true})).await.unwrap()}));
    }
    for job in jobs {
        job.await.unwrap();
    }
    assert_eq!(
        core.execute(json!({"op":"library"})).await.unwrap()["items"]
            .as_array()
            .unwrap()
            .len(),
        24
    );
    let (tx, rx) = std::sync::mpsc::channel();
    let notify = Mutex::new(Some(tx));
    let s = Server::new(move |p| {
        if p.contains("manifest.json") {
            Reply::ok(manifest())
        } else {
            if let Some(tx) = notify.lock().unwrap().take() {
                tx.send(()).unwrap();
            }
            thread::sleep(std::time::Duration::from_millis(250));
            Reply::ok(json!({"metas":[]}))
        }
    });
    empty(&core).await;
    install(&core, &s).await;
    let c = core.clone();
    let url = s.url();
    let net = tokio::spawn(async move {
        c.execute(json!({"op":"catalog","addon_url":url,"type":"movie","id":"top"}))
            .await
            .unwrap()
    });
    tokio::task::spawn_blocking(move || {
        rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap()
    })
    .await
    .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_millis(100),
        core.execute(json!({"op":"library"})),
    )
    .await
    .unwrap()
    .unwrap();
    net.await.unwrap();
}

#[tokio::test]
async fn metadata_refresh_ttl_and_inline_stream_security() {
    let version = Arc::new(AtomicUsize::new(1));
    let current = version.clone();
    let s = Server::new(move |p| {
        if p.contains("manifest.json") {
            let mut m = manifest();
            m["version"] = json!(format!("{}.0.0", current.load(Ordering::SeqCst)));
            Reply::ok(m)
        } else if p.contains("/meta/") {
            Reply::ok(json!({"meta":{
                "id":"tt9","type":"series","name":"Episode title","unknown":{"still":"raw"},
                "videos":[{"id":"tt9:1:1","title":"Raw\nEpisode","streams":[{"url":"https://example.test/video?inline-secret-never-persist=1","name":"Raw\nName"}]}]
            }}))
        } else {
            Reply {
                status: 200,
                body: json!({"metas":[]}),
                cache: "max-age=0",
                location: None,
            }
        }
    });
    let d = Data::new();
    let core = d.core();
    empty(&core).await;
    install(&core, &s).await;
    let req = json!({"op":"meta","addon_url":s.url(),"type":"series","id":"tt9"});
    let fresh = core.execute(req.clone()).await.unwrap();
    assert_eq!(
        fresh["meta"]["videos"][0]["streams"][0]["name"],
        "Raw\nName"
    );
    let second = core.execute(req.clone()).await.unwrap();
    assert_eq!(second["cached"], false);
    assert_eq!(second["meta"]["unknown"]["still"], "raw");
    assert_eq!(
        second["meta"]["videos"][0]["streams"],
        fresh["meta"]["videos"][0]["streams"]
    );
    assert_eq!(s.count("/meta/"), 2);
    let mut forced = req;
    forced["force"] = json!(true);
    assert_eq!(core.execute(forced).await.unwrap()["cached"], false);
    assert_eq!(s.count("/meta/"), 3);
    assert_eq!(db_count(&d, "cache"), 0);
    core.execute(json!({"op":"set_library","type":"series","id":"tt9","addon_url":s.url(),"saved":true,"meta":fresh["meta"]})).await.unwrap();
    for name in ["core.sqlite3", "core.sqlite3-wal"] {
        if let Ok(bytes) = std::fs::read(d.0.join(name)) {
            assert!(!bytes
                .windows(b"inline-secret-never-persist".len())
                .any(|w| w == b"inline-secret-never-persist"));
        }
    }
    for _ in 0..2 {
        assert_eq!(
            core.execute(json!({"op":"catalog","addon_url":s.url(),"type":"movie","id":"top"}))
                .await
                .unwrap()["cached"],
            false
        );
    }
    assert_eq!(s.count("/catalog/"), 2);
    version.store(2, Ordering::SeqCst);
    let updated = core.execute(json!({"op":"refresh_addons"})).await.unwrap();
    assert_eq!(updated["addons"][0]["version"], "2.0.0");
    assert_eq!(updated["addons"][0]["unknown"]["preserve"], true);
    drop(core);
    assert_eq!(
        d.core().execute(json!({"op":"addons"})).await.unwrap()["addons"][0]["version"],
        "2.0.0"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn addon_errors_are_independent_and_fanout_is_bounded() {
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let (a, m) = (active.clone(), maximum.clone());
    let s = Server::new(move |p| {
        if p.contains("manifest.json") {
            return Reply::ok(manifest());
        }
        let now = a.fetch_add(1, Ordering::SeqCst) + 1;
        m.fetch_max(now, Ordering::SeqCst);
        thread::sleep(std::time::Duration::from_millis(40));
        a.fetch_sub(1, Ordering::SeqCst);
        if p.contains("/configured/0/") {
            Reply {
                status: 503,
                body: json!({}),
                cache: "no-store",
                location: None,
            }
        } else {
            Reply::ok(
                json!({"streams":[{"title":"Kept\nverbatim","url":"https://example.test/legal-video.mp4"}]}),
            )
        }
    });
    let d = Data::new();
    let core = d.core();
    empty(&core).await;
    for n in 0..12 {
        let url = s
            .url()
            .replace("/configured/", &format!("/configured/{n}/"));
        core.execute(json!({"op":"install_addon","url":url}))
            .await
            .unwrap();
    }
    let result = core
        .execute(json!({"op":"streams","type":"series","id":"tt123"}))
        .await
        .unwrap();
    let groups = result["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 12);
    assert!(groups[0].get("error").is_some());
    assert_eq!(groups[0]["streams"], json!([]));
    for g in &groups[1..] {
        assert!(g.get("error").is_none());
        assert_eq!(g["streams"][0]["title"], "Kept\nverbatim");
    }
    assert!((2..=8).contains(&maximum.load(Ordering::SeqCst)));
    assert_eq!(active.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn episode_progress_feedback_validation_and_transaction_rollback() {
    let d = Data::new();
    let core = d.core();
    let addon = "https://example.test/manifest.json";
    for (video, watched) in [
        ("kitsu:1:1", 12000),
        ("kitsu:1:2", 10000),
        ("kitsu:1:1", 1000),
    ] {
        core.execute(json!({"op":"progress","type":"anime","id":"kitsu:1","video_id":video,"addon_url":addon,"position_ms":20000,"duration_ms":100000,"watched_ms":watched,"meta":{"name":"Anime"}})).await.unwrap();
    }
    let lib = core.execute(json!({"op":"library"})).await.unwrap();
    assert_eq!(lib["items"][0]["watched_ms"], 22000);
    assert_eq!(lib["items"][0]["watched"], false);
    for (op, extra) in [
        ("feedback", json!({"value":2})),
        ("progress", json!({"position_ms":-1,"duration_ms":100})),
    ] {
        let mut req = json!({"op":op,"type":"movie","id":"tt999","addon_url":addon});
        req.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        assert!(core.execute(req).await.is_err());
    }
    assert_eq!(db_count(&d, "media"), 1);
    assert_eq!(core.execute(json!({"op":"mark_watched","type":"anime","id":"kitsu:1","addon_url":addon,"watched":true})).await.unwrap()["item"]["watched"], true);
    assert_eq!(core.execute(json!({"op":"mark_watched","type":"anime","id":"kitsu:1","addon_url":addon,"watched":false})).await.unwrap()["item"]["watched"], false);
    core.execute(json!({"op":"mark_videos","type":"anime","id":"kitsu:1","addon_url":addon,"watched":true,"video_ids":["kitsu:1:1","kitsu:1:2"]})).await.unwrap();
    let wv = core.execute(json!({"op":"watched_videos","type":"anime","id":"kitsu:1"})).await.unwrap();
    assert_eq!(wv["video_ids"].as_array().unwrap().len(), 2);
    core.execute(json!({"op":"mark_videos","type":"anime","id":"kitsu:1","addon_url":addon,"watched":false,"video_ids":["kitsu:1:2"]})).await.unwrap();
    assert_eq!(core.execute(json!({"op":"watched_videos","type":"anime","id":"kitsu:1"})).await.unwrap()["video_ids"], json!(["kitsu:1:1"]));
    assert!(core.execute(json!({"op":"mark_videos","type":"anime","id":"kitsu:1","addon_url":addon,"video_ids":[""]})).await.is_err());
    core.execute(json!({"op":"forget","type":"anime","id":"kitsu:1","addon_url":addon}))
        .await
        .unwrap();
    assert_eq!(db_count(&d, "media"), 0);
    assert_eq!(
        core.execute(json!({"op":"library"})).await.unwrap()["items"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
}

#[tokio::test]
async fn completion_is_per_video_uses_retained_evidence_and_keeps_movie_behavior() {
    let d = Data::new();
    let core = d.core();
    let addon = "https://example.test/manifest.json";
    for kind in ["series", "anime", "movie"] {
        let mut progress = json!({"op":"progress","type":kind,"id":"opaque","addon_url":addon,"video_id":"ep1","position_ms":70000,"duration_ms":100000,"watched_ms":85000});
        let first = core.execute(progress.clone()).await.unwrap();
        assert_eq!(first["item"]["video_watched"], false);
        progress["position_ms"] = json!(95000);
        progress.as_object_mut().unwrap().remove("watched_ms");
        let completed = core.execute(progress.clone()).await.unwrap();
        assert_eq!(completed["item"]["video_watched"], true);
        assert_eq!(completed["item"]["watched"], kind == "movie");
        progress["position_ms"] = json!(1000);
        progress["watched_ms"] = json!(100);
        assert_eq!(
            core.execute(progress.clone()).await.unwrap()["item"]["video_watched"],
            true
        );
        if kind != "movie" {
            progress["video_id"] = json!("ep2");
            let next = core.execute(progress.clone()).await.unwrap();
            assert_eq!(next["item"]["watched"], false);
            assert_eq!(next["item"]["video_watched"], false);
            progress["position_ms"] = json!(80000);
            progress["watched_ms"] = json!(85000);
            core.execute(progress.clone()).await.unwrap();
            progress["position_ms"] = json!(95000);
            progress["watched_ms"] = json!(100);
            assert_eq!(
                core.execute(progress).await.unwrap()["item"]["video_watched"],
                true
            );
        }
    }
}

#[tokio::test]
async fn v1_migration_preserves_history_manual_marks_cache_and_addons() {
    let d = Data::new();
    let db = d.db();
    db.execute_batch("
        CREATE TABLE addons(url TEXT PRIMARY KEY,manifest TEXT NOT NULL,allow_http INTEGER NOT NULL DEFAULT 0);
        CREATE TABLE cache(key TEXT PRIMARY KEY,body TEXT NOT NULL,expires INTEGER NOT NULL,updated INTEGER NOT NULL);
        CREATE TABLE media(key TEXT PRIMARY KEY,type TEXT NOT NULL,raw_id TEXT NOT NULL,addon_url TEXT NOT NULL,meta TEXT NOT NULL,saved INTEGER NOT NULL DEFAULT 0,watched INTEGER NOT NULL DEFAULT 0,position_ms INTEGER NOT NULL DEFAULT 0,duration_ms INTEGER NOT NULL DEFAULT 0,watched_ms INTEGER NOT NULL DEFAULT 0,liked INTEGER NOT NULL DEFAULT 0,video_id TEXT,updated INTEGER NOT NULL);
        CREATE TABLE playback(media_key TEXT NOT NULL REFERENCES media(key) ON DELETE CASCADE,video_id TEXT NOT NULL,position_ms INTEGER NOT NULL,duration_ms INTEGER NOT NULL,watched_ms INTEGER NOT NULL,updated INTEGER NOT NULL,PRIMARY KEY(media_key,video_id));
        PRAGMA user_version=1;
    ").unwrap();
    let addon = "https://example.test/manifest.json";
    db.execute(
        "INSERT INTO addons VALUES(?1,?2,1)",
        rusqlite::params![addon, manifest().to_string()],
    )
    .unwrap();
    db.execute("INSERT INTO cache VALUES('retained','{}',123,100)", [])
        .unwrap();
    for (kind, id, video, pos, evidence, watched) in [
        ("series", "tt9", "tt9:1:1", 95000, 85000, true),
        ("movie", "tt8", "tt8", 95000, 85000, true),
        ("anime", "kitsu:7", "kitsu:7:1", 95000, 0, false),
    ] {
        let key = json!([kind, id, ""]).to_string();
        db.execute("INSERT INTO media(key,type,raw_id,addon_url,meta,saved,watched,position_ms,duration_ms,watched_ms,liked,video_id,updated) VALUES(?1,?2,?3,?4,?5,1,?6,?7,100000,?8,1,?9,100)",rusqlite::params![key,kind,id,addon,json!({"name":"Retained"}).to_string(),watched,pos,evidence,video]).unwrap();
        db.execute(
            "INSERT INTO playback VALUES(?1,?2,?3,100000,?4,100)",
            rusqlite::params![key, video, pos, evidence],
        )
        .unwrap();
    }
    drop(db);
    for _ in 0..2 {
        let core = d.core();
        let version: i64 = d.db().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, 3);
        assert_eq!(db_count(&d, "addons"), 1);
        assert_eq!(db_count(&d, "cache"), 1);
        assert_eq!(db_count(&d, "playback"), 3);
        let lib = core.execute(json!({"op":"library"})).await.unwrap();
        assert_eq!(lib["items"].as_array().unwrap().len(), 3);
        for m in lib["items"].as_array().unwrap() {
            assert_eq!(m["library"], true);
            assert_eq!(m["liked"], 1);
            assert_eq!(m["name"], "Retained");
            assert_eq!(m["video_watched"], true);
            assert_eq!(m["watched"], m["type"] != "anime");
        }
    }
    let db = d.db();
    let (consent, generation): (bool, String) = db
        .query_row("SELECT allow_http,generation FROM addons", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert!(consent);
    assert_eq!(generation.len(), 32);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn refresh_does_not_resurrect_remove_or_overwrite_reinstall() {
    for action in ["remove", "remove-reinstall", "upsert"] {
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let notify = Mutex::new(Some(started_tx));
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let release = Mutex::new(release_rx);
        let s = Server::new(move |_| {
            let n = count.fetch_add(1, Ordering::SeqCst);
            if n == 1 {
                notify.lock().unwrap().take().unwrap().send(()).unwrap();
                release
                    .lock()
                    .unwrap()
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
            }
            let mut m = manifest();
            m["version"] = json!(format!("{}.0.0", n + 1));
            Reply::ok(m)
        });
        let d = Data::new();
        let core = d.core();
        empty(&core).await;
        install(&core, &s).await;
        let c = core.clone();
        let refresh =
            tokio::spawn(async move { c.execute(json!({"op":"refresh_addons"})).await.unwrap() });
        tokio::task::spawn_blocking(move || {
            started_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap()
        })
        .await
        .unwrap();
        if action != "upsert" {
            core.execute(json!({"op":"remove_addon","url":s.url()}))
                .await
                .unwrap();
        }
        if action != "remove" {
            core.execute(json!({"op":"install_addon","url":s.url(),"allow_http":true}))
                .await
                .unwrap();
        }
        release_tx.send(()).unwrap();
        let refreshed = refresh.await.unwrap();
        assert!(refreshed["results"][0]["error"]
            .as_str()
            .unwrap()
            .contains("stale response ignored"));
        if action == "remove" {
            assert_eq!(refreshed["addons"], json!([]));
        } else {
            assert_eq!(refreshed["addons"][0]["version"], "3.0.0");
            let db = d.db();
            assert!(db
                .query_row("SELECT allow_http FROM addons", [], |r| r.get::<_, bool>(0))
                .unwrap());
        }
        drop(core);
        let persisted = d.core().execute(json!({"op":"addons"})).await.unwrap();
        assert_eq!(persisted["addons"], refreshed["addons"]);
    }
}

#[tokio::test]
async fn repeated_cache_control_headers_apply_restrictive_policy_to_disk_and_memory() {
    for control in [
        "max-age=3600\r\nCache-Control: no-store",
        "max-age=3600\r\nCache-Control: private=\"Set-Cookie\"",
        "max-age=3600\r\nCache-Control: no-cache",
        "public\r\nCache-Control: max-age = \"0\"",
        "max-age=3600\r\nCache-Control: max-age=0",
    ] {
        let s = Server::new(move |p| {
            if p.contains("manifest.json") {
                return Reply::ok(manifest());
            }
            let body = if p.contains("/stream/") {
                json!({"streams":[{"url":"https://example.test/video"}]})
            } else {
                json!({"metas":[{"id":"tt1","name":"Cache check"}]})
            };
            Reply {
                status: 200,
                body,
                cache: control,
                location: None,
            }
        });
        let d = Data::new();
        let core = d.core();
        empty(&core).await;
        install(&core, &s).await;
        for _ in 0..2 {
            assert_eq!(
                core.execute(json!({"op":"catalog","addon_url":s.url(),"type":"movie","id":"top"}))
                    .await
                    .unwrap()["cached"],
                false,
                "{control}"
            );
            core.execute(json!({"op":"streams","type":"series","id":"tt1"}))
                .await
                .unwrap();
        }
        assert_eq!(s.count("/catalog/"), 2, "{control}");
        assert_eq!(s.count("/stream/"), 2, "{control}");
        if control.contains("no-store") || control.contains("private") {
            assert_eq!(db_count(&d, "cache"), 0, "{control}");
            assert_eq!(db_count(&d, "media"), 0, "{control}");
        }
    }
}

#[tokio::test]
async fn exact_last_position_survives_restart_for_movies_series_and_anime() {
    let d = Data::new();
    let core = d.core();
    let addon = "https://resume.test/manifest.json";
    let titles = [
        ("movie", "tt9001", "tt9001"),
        ("series", "tt9002", "tt9002:2:7"),
        ("anime", "kitsu:9003", "kitsu:9003:12"),
    ];
    for (kind, id, video) in titles {
        if kind != "movie" {
            core.execute(json!({"op":"progress","type":kind,"id":id,"addon_url":addon,"video_id":"previous-episode","position_ms":98765,"duration_ms":200000})).await.unwrap();
        }
        for position in [12345, 67890, 23456] {
            core.execute(json!({"op":"progress","type":kind,"id":id,"addon_url":addon,"video_id":video,"position_ms":position,"duration_ms":200000,"watched_ms":10000})).await.unwrap();
        }
        core.execute(json!({"op":"set_library","type":kind,"id":id,"addon_url":addon,"saved":true,"meta":{"name":"Refreshed title"}})).await.unwrap();
    }
    drop(core);
    let core = d.core();
    for (kind, id, video) in titles {
        let library = core.execute(json!({"op":"library"})).await.unwrap();
        let item = library["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["id"] == id)
            .unwrap();
        assert_eq!(item["video_id"], video);
        assert_eq!(item["position_ms"], 23456);
        if kind != "movie" {
            let key = json!([kind, id, ""]).to_string();
            let prev_pos: i64 = d
                .db()
                .query_row(
                    "SELECT position_ms FROM playback WHERE media_key=?1 AND video_id='previous-episode'",
                    [key],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(prev_pos, 98765);
        }
    }
}

#[tokio::test]
async fn metadata_free_mutations_preserve_exact_json_provenance_and_return_state() {
    let d = Data::new();
    let core = d.core();
    let a = "https://a.test/manifest.json";
    let b = "https://b.test/manifest.json";
    let meta = json!({"name":"Kept","genres":["Drama"],"unknown":{"keep":true},
        "collection":{"id":"saga","name":"Source A"},
        "videos":[{"id":"ep1","streams":[{"url":"https://secret.test/do-not-store"}]}]});
    let first = core.execute(json!({"op":"set_library","type":"series","id":"tt77","addon_url":a,"saved":true,"meta":meta})).await.unwrap();
    assert!(first["item"]["videos"][0].get("streams").is_none());
    let key = json!(["series", "tt77", ""]).to_string();
    let db = d.db();
    let old: String = db
        .query_row("SELECT meta FROM media WHERE key=?1", [&key], |r| r.get(0))
        .unwrap();
    let old = serde_json::to_string_pretty(&serde_json::from_str::<Value>(&old).unwrap()).unwrap();
    db.execute(
        "UPDATE media SET meta=?2 WHERE key=?1",
        rusqlite::params![key, old],
    )
    .unwrap();
    db.execute_batch("CREATE TRIGGER forbid_meta_update BEFORE UPDATE OF meta ON media BEGIN SELECT RAISE(ABORT,'metadata rewritten'); END;").unwrap();
    for empty in [false, true] {
        for (op, fields) in [
            (
                "progress",
                json!({"position_ms":95000,"duration_ms":100000,"watched_ms":85000,"video_id":"ep1"}),
            ),
            ("set_library", json!({"saved":false})),
            ("mark_watched", json!({"watched":true})),
            ("feedback", json!({"value":-1})),
        ] {
            let mut req = json!({"op":op,"type":"series","id":"tt77","addon_url":b});
            req.as_object_mut()
                .unwrap()
                .extend(fields.as_object().unwrap().clone());
            if empty {
                req["meta"] = json!({});
            }
            let result = core.execute(req).await.unwrap();
            assert_eq!(result["item"]["addon_url"], a);
            for field in ["name", "genres", "unknown", "collection", "videos"] {
                assert_eq!(result["item"][field], first["item"][field]);
            }
            let library = core.execute(json!({"op":"library"})).await.unwrap();
            assert_eq!(library["items"][0], result["item"]);
            assert_eq!(
                db.query_row("SELECT meta FROM media WHERE key=?1", [&key], |r| r
                    .get::<_, String>(0))
                    .unwrap(),
                old
            );
        }
    }
    db.execute_batch("DROP TRIGGER forbid_meta_update;")
        .unwrap();
    let identity_only = core.execute(json!({"op":"set_library","type":"series","id":"tt77","addon_url":b,"saved":true,"meta":{"id":"wrong","type":"wrong"}})).await.unwrap();
    assert_eq!(identity_only["item"]["addon_url"], a);
    assert_eq!(identity_only["item"]["id"], "tt77");
    assert_eq!(identity_only["item"]["type"], "series");
    let switched = core.execute(json!({"op":"set_library","type":"series","id":"tt77","addon_url":b,"saved":true,"meta":{"name":null}})).await.unwrap();
    assert_eq!(switched["item"]["addon_url"], b);
    assert_eq!(switched["item"]["name"], "Kept");
    assert!(switched["item"].get("collection").is_none());
    for bad in [json!(null), json!([]), json!(""), json!(false)] {
        assert!(core
            .execute(
                json!({"op":"progress","type":"series","id":"tt77","addon_url":b,
            "position_ms":0,"duration_ms":100000,"meta":bad})
            )
            .await
            .is_err());
    }
    drop(core);
    assert_eq!(
        d.core().execute(json!({"op":"library"})).await.unwrap()["items"][0],
        switched["item"]
    );
}

#[tokio::test]
async fn metadata_free_new_rows_return_minimal_items_and_remain_identity_scoped() {
    let d = Data::new();
    let core = d.core();
    let a = "https://a.test/manifest.json";
    let b = "https://b.test/manifest.json";
    for (i, (kind, id, addon)) in [
        ("movie", "opaque", a),
        ("movie", "opaque", b),
        ("series", "opaque", a),
        ("anime", "kitsu:7", a),
        ("anime", "kitsu:7", b),
        ("movie", "tt7", a),
        ("movie", "tt7", b),
        ("series", "tt7", a),
    ]
    .into_iter()
    .enumerate()
    {
        let mut req = json!({"op":"progress","type":kind,"id":id,"addon_url":addon,
            "position_ms":12345+i,"duration_ms":100000});
        if i % 2 == 0 {
            req["meta"] = json!({});
        }
        let item = core.execute(req).await.unwrap();
        let source = if (kind, id) == ("anime", "kitsu:7") || (kind, id) == ("movie", "tt7") {
            a
        } else {
            addon
        };
        assert_eq!(
            item,
            json!({"item":{"id":id,"type":kind,"addon_url":source,"library":false,
            "watched":false,"video_watched":false,"position_ms":12345+i,"duration_ms":100000,
            "watched_ms":0,"liked":0,"video_id":id}})
        );
    }
    assert_eq!(db_count(&d, "media"), 6);
}

#[tokio::test]
async fn large_metadata_replacement_fits_the_resulting_row_budget() {
    let data = Data::new();
    let core = data.core();
    let request = |value: &str| {
        json!({"op":"set_library","type":"series","id":"tt9990001",
        "addon_url":"https://fixture.invalid/manifest.json","saved":true,
        "meta":{"name":"Large metadata","description":value}})
    };
    core.execute(request(&"A".repeat(600_000))).await.unwrap();
    let result = core.execute(request(&"B".repeat(600_000))).await.unwrap();
    assert_eq!(result["item"]["description"], "B".repeat(600_000));
    let oversized = core.execute(request(&"C".repeat(1_100_000))).await.unwrap();
    assert_eq!(oversized["item"]["description"], "B".repeat(600_000));
}

#[tokio::test]
async fn measured_time_adds_up_across_sessions_and_unmarking_resets_the_total() {
    let d = Data::new();
    let core = d.core();
    let a = "https://a.test/manifest.json";
    let film = |position: i64, add: i64| json!({"op":"progress","type":"movie","id":"tt9","addon_url":a,"position_ms":position,"duration_ms":100000,"watched_add_ms":add,"meta":{"name":"F"}});
    let first = core.execute(film(45000, 45000)).await.unwrap();
    assert_eq!(first["item"]["watched"], false);
    core.execute(film(50000, 0)).await.unwrap();
    let done = core.execute(film(95000, 45000)).await.unwrap();
    assert_eq!(done["item"]["watched_ms"], 90000);
    assert_eq!(done["item"]["watched"], true);
    let skip = core.execute(json!({"op":"progress","type":"movie","id":"tt8","addon_url":a,"position_ms":99000,"duration_ms":100000,"watched_add_ms":3000})).await.unwrap();
    assert_eq!(skip["item"]["watched"], false);
    let big = core.execute(json!({"op":"progress","type":"movie","id":"tt7","addon_url":a,"position_ms":10,"duration_ms":1000,"watched_add_ms":999999})).await.unwrap();
    assert_eq!(big["item"]["watched_ms"], 1000);

    let ep = |video: &str| json!({"op":"progress","type":"series","id":"tt5","video_id":video,"addon_url":a,"position_ms":95000,"duration_ms":100000,"watched_add_ms":90000});
    core.execute(ep("tt5:1:1")).await.unwrap();
    core.execute(ep("tt5:1:2")).await.unwrap();
    let watched = core
        .execute(json!({"op":"watched_videos","type":"series","id":"tt5"}))
        .await
        .unwrap();
    let mut ids: Vec<&str> = watched["video_ids"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    ids.sort();
    assert_eq!(ids, ["tt5:1:1", "tt5:1:2"]);
    let after = core.execute(json!({"op":"mark_videos","type":"series","id":"tt5","addon_url":a,"watched":false,"video_ids":["tt5:1:1"]})).await.unwrap();
    assert_eq!(after["item"]["watched_ms"], 90000);
    let watched = core
        .execute(json!({"op":"watched_videos","type":"series","id":"tt5"}))
        .await
        .unwrap();
    assert_eq!(watched["video_ids"], json!(["tt5:1:2"]));

    let history = core.execute(json!({"op":"history"})).await.unwrap();
    let items = history["items"].as_array().unwrap();
    assert_eq!(items.len(), 4);
    assert!(items
        .iter()
        .all(|i| i["seen"].as_i64().unwrap() > 0 && i["type"].is_string()));
}

#[tokio::test]
async fn removing_an_addon_drops_its_cached_responses() {
    let s = Server::new(|p| {
        if p.contains("manifest.json") {
            Reply::ok(manifest())
        } else {
            Reply::ok(json!({"metas":[{"id":"tt1","type":"movie","name":"A"}]}))
        }
    });
    let d = Data::new();
    let core = d.core();
    empty(&core).await;
    install(&core, &s).await;
    core.execute(json!({"op":"catalog","addon_url":s.url(),"type":"movie","id":"top"}))
        .await
        .unwrap();
    assert_eq!(db_count(&d, "cache"), 1);
    core.execute(json!({"op":"remove_addon","url":s.url()}))
        .await
        .unwrap();
    assert_eq!(db_count(&d, "cache"), 0);
    assert_eq!(
        core.execute(json!({"op":"addons"})).await.unwrap()["addons"],
        json!([])
    );
}

#[tokio::test]
async fn an_unmarked_film_is_not_marked_again_by_the_next_report() {
    let d = Data::new();
    let core = d.core();
    let a = "https://a.test/manifest.json";
    let report = |position: i64, add: i64| json!({"op":"progress","type":"movie","id":"tt3","addon_url":a,"position_ms":position,"duration_ms":100000,"watched_add_ms":add});
    assert_eq!(
        core.execute(report(85000, 84000)).await.unwrap()["item"]["watched"],
        true
    );
    let unmarked = core
        .execute(
            json!({"op":"mark_watched","type":"movie","id":"tt3","addon_url":a,"watched":false}),
        )
        .await
        .unwrap();
    assert_eq!(unmarked["item"]["watched"], false);
    assert_eq!(unmarked["item"]["watched_ms"], 0);
    assert_eq!(
        core.execute(report(10000, 10000)).await.unwrap()["item"]["watched"],
        false
    );
}

#[tokio::test]
async fn old_history_played_past_80_percent_stays_watched_after_the_upgrade() {
    let d = Data::new();
    drop(d.core());
    let db = rusqlite::Connection::open(d.0.join("core.sqlite3")).unwrap();
    let addon = "https://example.test/manifest.json";
    for (kind, id, video, position, measured) in [
        ("movie", "tt1", "tt1", 85000, 0),
        ("series", "tt2", "tt2:1:1", 82000, 0),
        ("movie", "tt3", "tt3", 50000, 0),
        ("movie", "tt4", "tt4", 85000, 10000),
    ] {
        let key = json!([kind, id, ""]).to_string();
        db.execute("INSERT INTO media(key,type,raw_id,addon_url,meta,position_ms,duration_ms,watched_ms,video_id,updated) VALUES(?1,?2,?3,?4,'{}',?5,100000,?7,?6,1)",
            rusqlite::params![key, kind, id, addon, position, video, measured]).unwrap();
        db.execute("INSERT INTO playback(media_key,video_id,position_ms,duration_ms,watched_ms,updated) VALUES(?1,?2,?3,100000,?4,1)",
            rusqlite::params![key, video, position, measured]).unwrap();
    }
    db.execute_batch("PRAGMA user_version=2;").unwrap();
    drop(db);
    let core = d.core();
    let item = |id: &'static str, kind: &'static str| {
        let core = core.clone();
        async move {
            core.execute(json!({"op":"item","type":kind,"id":id,"addon_url":addon}))
                .await
                .unwrap()["item"]
                .clone()
        }
    };
    assert_eq!(item("tt1", "movie").await["watched"], true);
    assert_eq!(item("tt3", "movie").await["watched"], false);
    assert_eq!(item("tt4", "movie").await["watched"], false);
    let videos = core
        .execute(json!({"op":"watched_videos","type":"series","id":"tt2"}))
        .await
        .unwrap();
    assert_eq!(videos["video_ids"], json!(["tt2:1:1"]));
    assert_eq!(
        core.execute(json!({"op":"item","type":"movie","id":"tt9","addon_url":addon}))
            .await
            .unwrap()["item"],
        Value::Null
    );
}

#[tokio::test]
async fn removing_an_addon_keeps_its_neighbours_cache() {
    let d = Data::new();
    drop(d.core());
    let gone = "https://host.test/manifest.json?key=a";
    d.install(gone, &manifest());
    let db = d.db();
    for key in [
        "https://host.test/catalog/movie/top.json?key=a",
        "https://host.test/catalog/movie/top.json?key=b",
        "https://host.test/sort=x|rd=K/catalog/movie/top.json",
        "https://other.test/catalog/movie/top.json?key=a",
    ] {
        db.execute(
            "INSERT INTO cache(key,body,expires,updated) VALUES(?1,'{}',9999999999,1)",
            [key],
        )
        .unwrap();
    }
    drop(db);
    let core = d.core();
    core.execute(json!({"op":"remove_addon","url":gone}))
        .await
        .unwrap();
    let left: Vec<String> = d
        .db()
        .prepare("SELECT key FROM cache ORDER BY key")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(
        left,
        [
            "https://host.test/catalog/movie/top.json?key=b",
            "https://host.test/sort=x|rd=K/catalog/movie/top.json",
            "https://other.test/catalog/movie/top.json?key=a",
        ]
    );
}
