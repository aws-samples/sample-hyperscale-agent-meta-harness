//! Partial content, binary types and other hosts are left alone; a textual body the proxy cannot rewrite is refused, not
//! leaked.
#![allow(clippy::unwrap_used)]

mod common;
use bytes::Bytes;
use common::*;
use std::time::{Duration, Instant};

fn html(body: String) -> Reply {
    Reply::ok(body).header("Content-Type", "text/html; charset=utf-8")
}

#[tokio::test]
async fn pypi_links_to_the_upstream_become_links_to_the_proxy() {
    let page = format!(
        "<html><body><a href=\"https://{CA_HOST}/pypi/packages/simple/demo/demo-1.0-py3-none-any.whl#sha256=abc\">demo</a>\
         <a href=\"../../files/x.tar.gz\">relative</a><a href=\"https://elsewhere.example/pypi/packages/x\">other</a></body></html>"
    );
    let r = rig(always(html(page)), nothing(), nothing()).await;
    let p = r.addr().port();
    let resp = r.get("/pypi/simple/demo/").await;
    assert_eq!(resp.status, 200);
    let body = resp.text();
    assert!(
        body.contains(&format!(
            "href=\"http://127.0.0.1:{p}/pypi/simple/demo/demo-1.0-py3-none-any.whl#sha256=abc\""
        )),
        "{body}"
    );
    assert!(
        body.contains("href=\"../../files/x.tar.gz\""),
        "relative links stay: {body}"
    );
    assert!(
        body.contains("https://elsewhere.example/pypi/packages/x"),
        "another host is not rewritten: {body}"
    );
    assert!(!body.contains(CA_HOST), "{body}");
    assert!(
        !body.contains("None"),
        "the spike's http://None/ rewrite bug: {body}"
    );
    assert!(resp.complete);
    assert_eq!(
        resp.header("content-length"),
        Some(body.len().to_string().as_str())
    );
}

#[tokio::test]
async fn npm_tarball_urls_are_rewritten_plain_and_json_escaped() {
    let doc = format!(
        r#"{{"name":"left-pad","versions":{{"1.0.0":{{"dist":{{"tarball":"https://{CA_HOST}/npm/packages/left-pad/-/left-pad-1.0.0.tgz","shasum":"x"}}}}}},"esc":"https:\/\/{CA_HOST}\/npm\/packages\/left-pad\/-\/left-pad-2.0.0.tgz"}}"#
    );
    let r = rig(
        always(Reply::ok(doc).header("Content-Type", "application/json")),
        nothing(),
        nothing(),
    )
    .await;
    let p = r.addr().port();
    let resp = r.get("/npm/left-pad").await;
    let body = resp.text();
    assert!(
        body.contains(&format!(
            "\"http://127.0.0.1:{p}/npm/left-pad/-/left-pad-1.0.0.tgz\""
        )),
        "{body}"
    );
    assert!(
        body.contains(&format!(
            "http:\\/\\/127.0.0.1:{p}\\/npm\\/left-pad\\/-\\/left-pad-2.0.0.tgz"
        )),
        "the escaped form is rewritten too: {body}"
    );
    assert!(!body.contains(CA_HOST), "{body}");
    serde_free_json_check(&body);
}

/// A cheap well-formedness check without a JSON crate: braces and quotes balance.
fn serde_free_json_check(s: &str) {
    assert_eq!(s.matches('{').count(), s.matches('}').count(), "{s}");
    assert_eq!(s.matches('"').count() % 2, 0, "{s}");
}

#[tokio::test]
async fn a_url_that_only_starts_like_the_base_is_not_rewritten() {
    // `.../npm/packages2/` is another repository; the base is `.../npm/packages`.
    let doc = format!(
        "a https://{CA_HOST}/npm/packages2/x b https://{CA_HOST}/npm/packages-old/y c https://{CA_HOST}/npm/packages/z d https://{CA_HOST}/npm/packages end"
    );
    let r = rig(
        always(Reply::ok(doc).header("Content-Type", "text/plain")),
        nothing(),
        nothing(),
    )
    .await;
    let p = r.addr().port();
    let body = r.get("/npm/x").await.text();
    assert!(
        body.contains(&format!("https://{CA_HOST}/npm/packages2/x")),
        "{body}"
    );
    assert!(
        body.contains(&format!("https://{CA_HOST}/npm/packages-old/y")),
        "{body}"
    );
    assert!(
        body.contains(&format!("c http://127.0.0.1:{p}/npm/z")),
        "{body}"
    );
    assert!(
        body.contains(&format!("d http://127.0.0.1:{p}/npm end")),
        "{body}"
    );
}

#[tokio::test]
async fn maven_and_go_answers_are_never_rewritten_because_their_clients_checksum_them() {
    // A POM (and its .sha1) or a go.mod is hashed by the client; a rewritten byte would fail the check. Maven's mirror setting
    // already sends every repository URL in a POM to the proxy, and Go modules carry no repository URL.
    let pom = format!(
        "<project><repositories><repository><url>https://{CA_HOST}/maven/packages/</url></repository></repositories></project>"
    );
    let r = rig(
        always(
            Reply::ok(pom.clone())
                .header("Content-Type", "application/xml")
                .header("ETag", "\"v1\""),
        ),
        nothing(),
        always(
            Reply::ok(format!("module x\n// https://{GO_HOST}/x\n"))
                .header("Content-Type", "text/plain"),
        ),
    )
    .await;
    let resp = r.get("/maven/org/x/x-1.0.pom").await;
    assert_eq!(resp.text(), pom, "the POM reaches the client byte for byte");
    assert_eq!(
        resp.header("content-length"),
        Some(pom.len().to_string().as_str())
    );
    assert_eq!(
        resp.header("etag"),
        Some("\"v1\""),
        "an untouched answer keeps its validator"
    );
    let go = r.get("/go/example.com/m/@v/v1.0.0.mod").await;
    assert_eq!(go.text(), format!("module x\n// https://{GO_HOST}/x\n"));
}

#[tokio::test]
async fn a_rewritten_answer_does_not_promise_byte_ranges() {
    let page = format!("<a href=\"https://{CA_HOST}/pypi/packages/simple/x/\">x</a>");
    let r = rig(
        always(html(page).header("Accept-Ranges", "bytes")),
        nothing(),
        nothing(),
    )
    .await;
    let resp = r.get("/pypi/simple/x/").await;
    assert_eq!(resp.status, 200);
    assert!(!resp.text().contains(CA_HOST));
    assert_eq!(
        resp.header("accept-ranges"),
        None,
        "a range of the rewritten text would not match the upstream's bytes"
    );
}

#[tokio::test]
async fn the_pypi_json_and_npm_vendor_types_count_as_text() {
    for ct in [
        "application/vnd.pypi.simple.v1+json",
        "application/vnd.npm.install-v1+json",
        "application/json; charset=utf-8",
        "Application/JSON",
        "text/plain",
        "text/xml",
    ] {
        let r = rig(
            always(
                Reply::ok(format!("\"https://{CA_HOST}/pypi/packages/x\""))
                    .header("Content-Type", ct),
            ),
            nothing(),
            nothing(),
        )
        .await;
        let body = r.get("/pypi/x").await.text();
        assert!(!body.contains(CA_HOST), "{ct}: {body}");
    }
}

#[tokio::test]
async fn binary_partial_and_encoded_answers_are_left_untouched() {
    let url = format!("https://{CA_HOST}/pypi/packages/x");
    // Binary content type: bytes pass through exactly.
    let r = rig(
        always(Reply::ok(url.clone()).header("Content-Type", "application/octet-stream")),
        nothing(),
        nothing(),
    )
    .await;
    assert_eq!(r.get("/pypi/f.whl").await.text(), url);

    // A 206 of a text type: a rewrite would change the lengths the Content-Range promises.
    let mut part = Reply::ok(url.clone())
        .header("Content-Type", "text/html")
        .header("Content-Range", "bytes 0-9/100");
    part.status = 206;
    let r = rig(always(part), nothing(), nothing()).await;
    let resp = get_with(r.addr(), "/pypi/simple/x/", &[("Range", "bytes=0-9")]).await;
    assert_eq!((resp.status, resp.text()), (206, url.clone()));

    // Compressed binary is forwarded with its Content-Encoding.
    let r = rig(
        always(
            Reply::ok(Bytes::from_static(&[0x1f, 0x8b, 8, 0]))
                .header("Content-Type", "application/gzip")
                .header("Content-Encoding", "gzip"),
        ),
        nothing(),
        nothing(),
    )
    .await;
    let resp = r.get("/pypi/f.gz").await;
    assert_eq!(resp.header("content-encoding"), Some("gzip"));
    assert_eq!(resp.body, vec![0x1f, 0x8b, 8, 0]);
}

#[tokio::test]
async fn a_compressed_text_answer_is_refused_because_it_cannot_be_rewritten() {
    let r = rig(
        always(
            Reply::ok(Bytes::from_static(&[0x1f, 0x8b, 8, 0]))
                .header("Content-Type", "text/html")
                .header("Content-Encoding", "gzip"),
        ),
        nothing(),
        nothing(),
    )
    .await;
    let resp = r.get("/pypi/simple/x/").await;
    assert_eq!(resp.status, 502, "{}", resp.text());
    assert!(!resp.text().contains(CA_HOST));
}

#[tokio::test]
async fn a_text_answer_over_64_mib_is_refused() {
    let big = 65 * 1024 * 1024u64;
    // Declared length over the limit: refused before the body is read.
    let r = rig(
        always(
            Reply::ok("")
                .plan(Plan::Gen { total: big })
                .header("Content-Type", "text/html"),
        ),
        nothing(),
        nothing(),
    )
    .await;
    let resp = r.get("/pypi/simple/huge/").await;
    assert_eq!(resp.status, 502, "{}", resp.text());
    assert!(resp.text().contains("too large"), "{}", resp.text());

    // Unknown length (chunked): refused when the limit is crossed.
    let r = rig(
        always(
            Reply::ok("")
                .plan(Plan::Gen { total: big })
                .header("Content-Type", "text/html")
                .chunked(),
        ),
        nothing(),
        nothing(),
    )
    .await;
    let resp = r.get("/pypi/simple/huge/").await;
    assert_eq!(resp.status, 502, "{}", resp.text());

    // A 64 MiB answer is the largest that is rewritten.
    let ok = 64 * 1024 * 1024u64;
    let r = rig(
        always(
            Reply::ok("")
                .plan(Plan::Gen { total: ok })
                .header("Content-Type", "text/html"),
        ),
        nothing(),
        nothing(),
    )
    .await;
    let resp = r.get("/pypi/simple/limit/").await;
    assert_eq!((resp.status, resp.body.len() as u64), (200, ok));
}

#[tokio::test]
async fn head_of_a_text_resource_does_not_hang_and_has_no_body() {
    let r = rig(
        always(
            Reply::status(200)
                .header("Content-Type", "text/html")
                .header("Content-Length", "500"),
        ),
        nothing(),
        nothing(),
    )
    .await;
    let resp = head(r.addr(), "/pypi/simple/x/").await;
    assert_eq!(resp.status, 200);
    assert!(resp.body.is_empty());
    assert_eq!(r.ca.seen()[0].method, "HEAD");
}

#[tokio::test]
async fn only_a_few_text_answers_are_held_for_rewriting_at_once() {
    // Four slots. Four slow text answers (each alive: 100 ms between bytes) hold them; a fifth waits one idle period and gets 503.
    let mut o = Opts::new(
        always(
            Reply::ok("")
                .plan(Plan::Trickle {
                    gap: Duration::from_millis(100),
                    count: 12,
                })
                .header("Content-Type", "text/plain"),
        ),
        nothing(),
        nothing(),
    );
    o.idle_timeout = Duration::from_millis(400);
    let r = rig_with(o).await;
    let addr = r.addr();
    let mut holders = Vec::new();
    for i in 0..4 {
        holders.push(tokio::spawn(async move {
            get(addr, &format!("/pypi/slow{i}")).await
        }));
    }
    tokio::time::sleep(Duration::from_millis(250)).await;
    let fifth = get(addr, "/pypi/fifth").await;
    assert_eq!(fifth.status, 503, "{}", fifth.text());
    assert!(fifth.text().contains("busy"));
    for h in holders {
        let resp = h.await.unwrap();
        assert_eq!(
            (resp.status, resp.body.len()),
            (200, 12),
            "the four that hold a slot finish"
        );
    }
    // Slots are free again.
    assert_eq!(r.get("/pypi/after").await.status, 200);
    let s = r.proxy.snapshot();
    assert_eq!(s.statuses.get(&503), Some(&1), "{s:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_parallel_text_answers_all_get_through() {
    let page =
        format!("<a href=\"https://{CA_HOST}/pypi/packages/simple/x/\">x</a>").repeat(20_000);
    let r = rig(always(html(page.clone())), nothing(), nothing()).await;
    let addr = r.addr();
    let mut tasks = Vec::new();
    for i in 0..24 {
        tasks.push(tokio::spawn(async move {
            get(addr, &format!("/pypi/simple/p{i}/")).await
        }));
    }
    for t in tasks {
        let resp = t.await.unwrap();
        assert_eq!(resp.status, 200);
        assert!(resp.complete && !resp.text().contains(CA_HOST));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rewritten_answer_keeps_its_slot_until_it_has_been_sent() {
    // Four clients ask for a 24 MiB text answer and read it slowly (64 KiB every 100 ms, so the transfer would take about forty seconds and
    // the connection stays alive). The rewritten bodies sit unsent in the proxy, so the four slots stay taken: a fifth request waits
    // one idle period (8 s) and gets 503 "proxy busy". When the slow clients leave, the slots come back. A 502 for one of the four
    // would leave a slot free, so the idle time has to be longer than anything a loaded host does to the process: the runtime is blocked
    // for 2.6 s on purpose below, and the idle time must outlast it.
    let mut o = Opts::new(
        always(
            Reply::ok("")
                .plan(Plan::Gen {
                    total: 24 * 1024 * 1024,
                })
                .header("Content-Type", "text/plain"),
        ),
        nothing(),
        nothing(),
    );
    o.idle_timeout = Duration::from_secs(8);
    let r = rig_with(o).await;
    let addr = r.addr();
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let heads = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let mut readers = Vec::new();
    for i in 0..4 {
        let (stop, heads) = (stop.clone(), heads.clone());
        readers.push(tokio::spawn(async move {
            let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
            tokio::io::AsyncWriteExt::write_all(
                &mut s,
                &request("GET", &format!("/pypi/big{i}"), addr, &[]),
            )
            .await
            .unwrap();
            let mut buf = vec![0u8; 65536];
            let mut first = true;
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                let n = tokio::io::AsyncReadExt::read(&mut s, &mut buf)
                    .await
                    .unwrap_or(0);
                if n == 0 {
                    break;
                }
                if first {
                    // The answer is rewritten and its bytes are on their way: this reader's slot is taken.
                    first = false;
                    let line = String::from_utf8_lossy(&buf[..n.min(15)]).to_string();
                    heads.lock().unwrap().push(line);
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }));
    }
    // A loaded host (an instrumented coverage build rewriting 24 MiB four times over, on four workers) can keep every worker of the runtime
    // busy for longer than a short idle time. Everything the proxy has in flight then runs out its idle timer without the wire having
    // been idle, and the answer fails with 502 "upstream error: timeout". That leaves a slot free and failed this test once, with an
    // idle time of 2 s. The block is made here on purpose, once the proxy has
    // asked for all four answers, so the outcome does not depend on the speed of the host: 2.6 s is longer than 2 s, and with an idle
    // time of 2 s this test fails every time (10 of 10 runs).
    let asked = Instant::now();
    while r.upstream_requests() < 4 {
        assert!(
            asked.elapsed() < Duration::from_secs(60),
            "the proxy asked the upstream four times"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    for _ in 0..4 {
        drop(tokio::spawn(async {
            std::thread::sleep(Duration::from_millis(2600));
        }));
    }
    // Wait for all four slots to be taken, however slow the host is (an instrumented coverage build rewrites 24 MiB slowly); a
    // fixed pause let the fifth request take a slot itself.
    let waiting = Instant::now();
    while heads.lock().unwrap().len() < 4 {
        assert!(
            waiting.elapsed() < Duration::from_secs(60),
            "the four readers started"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        heads.lock().unwrap().iter().all(|h| h == "HTTP/1.1 200 OK"),
        "all four readers were served, so all four slots are taken: {:?}",
        heads.lock().unwrap()
    );
    let fifth = get(addr, "/pypi/fifth").await;
    assert_eq!(
        fifth.status,
        503,
        "the four unsent bodies still hold their slots (body bytes {})",
        fifth.body.len()
    );
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    for t in readers {
        let _ = t.await;
    }
    // The readers are gone; their connections close and the slots return.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let again = get(addr, "/pypi/again").await;
    assert_eq!((again.status, again.body.len()), (200, 24 * 1024 * 1024));
}

// -- Mutants R46 and R21 of the 2026-10-13 review ------------------------------------------------------------------------

#[tokio::test]
async fn the_64_mib_limit_is_exact_when_the_length_is_not_declared() {
    // Chunked answers have no declared length, so the running total is what enforces the limit: one byte over is refused, the
    // limit itself passes.
    let limit = 64 * 1024 * 1024u64;
    for (total, want) in [(limit, 200u16), (limit + 1, 502)] {
        let r = rig(
            always(
                Reply::ok("")
                    .plan(Plan::Gen { total })
                    .header("Content-Type", "text/html")
                    .chunked(),
            ),
            nothing(),
            nothing(),
        )
        .await;
        let resp = r.get("/pypi/simple/chunked-edge/").await;
        assert_eq!(resp.status, want, "{total} bytes: {}", resp.text().len());
        if want == 200 {
            assert_eq!(resp.body.len() as u64, limit);
        } else {
            assert!(resp.text().contains("too large"), "{}", resp.text());
        }
    }
}

#[tokio::test]
async fn a_compressed_text_answer_on_maven_or_go_passes_through_untouched() {
    // Only pypi and npm answers are read whole and rewritten (their indexes carry absolute links to themselves). A Maven or Go
    // answer is checksummed by its client, so it streams byte for byte, whatever its type or Content-Encoding: the same answer on
    // pypi is refused (`a_compressed_text_answer_is_refused_because_it_cannot_be_rewritten`).
    let packed = Bytes::from_static(b"\x1f\x8bnot-really-gzip, but opaque bytes");
    let r = rig(
        always(
            Reply::ok(packed.clone())
                .header("Content-Type", "text/xml")
                .header("Content-Encoding", "gzip"),
        ),
        nothing(),
        always(
            Reply::ok(packed.clone())
                .header("Content-Type", "text/plain")
                .header("Content-Encoding", "gzip"),
        ),
    )
    .await;
    for path in [
        "/maven/org/x/maven-metadata.xml",
        "/go/example.com/m/@v/list",
    ] {
        let resp = r.get(path).await;
        assert_eq!(resp.status, 200, "{path}: {}", resp.text());
        assert_eq!(resp.body, packed.to_vec(), "{path}");
        assert_eq!(resp.header("content-encoding"), Some("gzip"), "{path}");
    }
    assert_eq!(r.get("/pypi/simple/x/").await.status, 502);
}

#[tokio::test]
async fn a_maven_pom_still_matches_its_checksum_after_the_proxy() {
    // Why Maven is not rewritten (a deliberate narrowing of the contract's "every prefix", decided by this test): a Maven client
    // fetches `x.pom` and `x.pom.sha256` and compares them. The POM names its own repository, so a rewrite that covered Maven
    // would change the POM's bytes and the comparison would fail.
    use sha2::{Digest, Sha256};
    fn sha256_hex(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }
    let pom = format!(
        "<project><distributionManagement><repository><url>https://{CA_HOST}/maven/{REPO}/</url></repository></distributionManagement></project>"
    );
    let sum = sha256_hex(pom.as_bytes());
    let (pom2, sum2) = (pom.clone(), sum.clone());
    let r = rig(
        handler(move |s| {
            if s.target.ends_with(".sha256") {
                Reply::ok(sum2.clone()).header("Content-Type", "text/plain")
            } else {
                Reply::ok(pom2.clone()).header("Content-Type", "application/xml")
            }
        }),
        nothing(),
        nothing(),
    )
    .await;
    let got_pom = r.get("/maven/org/x/x/1.0/x-1.0.pom").await;
    let got_sum = r.get("/maven/org/x/x/1.0/x-1.0.pom.sha256").await;
    assert_eq!((got_pom.status, got_sum.status), (200, 200));
    assert_eq!(
        sha256_hex(&got_pom.body),
        got_sum.text(),
        "the checksum file verifies the POM as the client received it"
    );
    // TRIGGER PRESENT: a rewrite of the repository link would have broken that comparison.
    let local = format!("http://127.0.0.1:{}/maven", r.addr().port());
    let rewritten = pom.replace(&format!("https://{CA_HOST}/maven/{REPO}"), &local);
    assert_ne!(rewritten, pom);
    assert_ne!(sha256_hex(rewritten.as_bytes()), sum);
}

/// A JSON document of about `size` bytes made of npm version entries whose tarball links point at the upstream.
fn packument(size: usize) -> Bytes {
    let entry = format!(
        r#""6.2.2":{{"name":"vite","version":"6.2.2","dist":{{"tarball":"https://{CA_HOST}/npm/packages/vite/-/vite-6.2.2.tgz","shasum":"0123456789abcdef"}},"padding":"{}"}},"#,
        "x".repeat(900)
    );
    let mut out = String::with_capacity(size + entry.len());
    out.push_str(r#"{"name":"vite","versions":{"#);
    while out.len() < size {
        out.push_str(&entry);
    }
    out.push_str(r#""9.9.9":{}}}"#);
    Bytes::from(out)
}

#[tokio::test]
async fn a_40_mib_packument_is_rewritten_and_a_70_mib_one_is_refused_with_the_reason() {
    // Found in the first early gate run (2026-10-14): `npm view vite` asks for the full packument, 37.2 MiB at CodeArtifact, which
    // the 32 MiB cap refused. CodeArtifact sends it chunked, with no declared length.
    let doc = packument(40 * 1024 * 1024);
    let upstream_links = doc
        .windows(CA_HOST.len())
        .filter(|w| *w == CA_HOST.as_bytes())
        .count();
    assert!(upstream_links > 1000, "{upstream_links}");
    let r = rig(
        always(
            Reply::ok(doc.clone())
                .header("Content-Type", "application/json")
                .chunked(),
        ),
        nothing(),
        nothing(),
    )
    .await;
    let p = r.addr().port();
    let resp = r.get("/npm/vite").await;
    assert_eq!(
        resp.status,
        200,
        "{}",
        resp.text().chars().take(200).collect::<String>()
    );
    assert!(resp.complete);
    let body = resp.text();
    assert!(!body.contains(CA_HOST), "every upstream link was rewritten");
    assert_eq!(
        body.matches(&format!("http://127.0.0.1:{p}/npm/vite/-/vite-6.2.2.tgz"))
            .count(),
        upstream_links
    );
    // The rewrite shortens every link (`http://127.0.0.1:<port>` is shorter than the upstream's origin), so the answer is a little
    // smaller than the document, never much.
    assert!(
        body.len() > 40_000_000 && body.len() < doc.len(),
        "{} of {}",
        body.len(),
        doc.len()
    );
    drop(body);
    drop(r);

    // Over the cap, declared or not: the 502 keeps its reason, and the proxy goes on serving.
    for chunked in [false, true] {
        let mut reply = Reply::ok("")
            .plan(Plan::Gen {
                total: 70 * 1024 * 1024,
            })
            .header("Content-Type", "application/json");
        if chunked {
            reply = reply.chunked();
        }
        let r = rig(always(reply), nothing(), nothing()).await;
        let resp = r.get("/npm/vite").await;
        assert_eq!(resp.status, 502, "chunked={chunked}");
        assert!(
            resp.text().contains("response too large to rewrite"),
            "chunked={chunked}: {}",
            resp.text()
        );
        assert_eq!(r.get("/nope").await.status, 403, "still serving");
    }
}
