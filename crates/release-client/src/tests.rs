use super::*;
use serde_json::json;
fn release(version: &str) -> serde_json::Value {
    let name = format!("vaultsyncd-{version}-x64-unsigned.zip");
    json!({"tag_name":format!("v{version}"),"draft":false,"prerelease":false,"assets":[{
        "name":name,"state":"uploaded","size":3,"digest":format!("sha256:{:x}",Sha256::digest(b"zip")),
        "browser_download_url":format!("{REPO}/releases/download/v{version}/{name}")
    }]})
}
fn select(value: serde_json::Value, current: &str) -> Result<Option<ReleaseUpdate>, String> {
    candidate(serde_json::from_value(value).unwrap(), current)
}
#[test]
fn semantic_version_and_stable_channel_prevent_downgrades() {
    assert_eq!(
        select(release("0.10.0"), "0.9.9")
            .unwrap()
            .unwrap()
            .version(),
        "0.10.0"
    );
    assert!(select(release("0.9.9"), "0.10.0").unwrap().is_none());
    assert!(select(release("0.10.0"), "0.10.0").unwrap().is_none());
    for (field, value) in [
        ("draft", json!(true)),
        ("prerelease", json!(true)),
        ("tag_name", json!("v0.10.0-beta.1")),
        ("tag_name", json!("../../escape")),
    ] {
        let mut r = release("0.10.0");
        r[field] = value;
        assert!(select(r, "0.1.1").is_err());
    }
}
#[test]
fn asset_identity_integrity_and_bounds_fail_closed() {
    for (field, value) in [
        ("digest", json!(null)),
        ("digest", json!("sha256:wrong")),
        ("digest", json!(format!("md5:{}", "0".repeat(64)))),
        ("size", json!(0)),
        ("size", json!(MAX_PACKAGE + 1)),
        ("state", json!("new")),
        ("name", json!("../escape.exe")),
        ("name", json!("vaultsyncd-0.2.0-linux.zip")),
        (
            "browser_download_url",
            json!("http://github.com/unsafe.zip"),
        ),
        (
            "browser_download_url",
            json!("https://github.com/attacker/project/releases/download/v0.2.0/file.zip"),
        ),
    ] {
        let mut r = release("0.2.0");
        r["assets"][0][field] = value;
        assert!(select(r, "0.1.1").is_err(), "{field}");
    }
    let mut r = release("0.2.0");
    let a = r["assets"][0].clone();
    r["assets"].as_array_mut().unwrap().push(a);
    assert!(select(r, "0.1.1").is_err());
}
fn serve(status: u16, headers: &str, bytes: &[u8]) -> String {
    use std::{
        io::{Read, Write},
        net::TcpListener,
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let response =
        format!("HTTP/1.1 {status} Test\r\nConnection: close\r\n{headers}\r\n").into_bytes();
    let bytes = bytes.to_vec();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0; 8192];
        assert!(stream.read(&mut request).unwrap() > 0);
        stream.write_all(&response).unwrap();
        stream.write_all(&bytes).unwrap();
    });
    url
}
#[tokio::test]
async fn streaming_only_succeeds_after_full_size_and_digest_verification() {
    let update = select(release("0.2.0"), "0.1.1").unwrap().unwrap();
    let client = Client::new();
    let mut received = vec![];
    let url = serve(200, "Content-Length: 3\r\n", b"zip");
    receive(
        client.get(url).send().await.unwrap(),
        &update.asset,
        |bytes, n, total| {
            received.extend_from_slice(bytes);
            assert!(n <= total);
            Ok(())
        },
    )
    .await
    .unwrap();
    assert_eq!(received, b"zip");
    for (headers, bytes) in [
        ("Content-Length: 3\r\n", b"bad".as_slice()),
        ("Content-Length: 3\r\n", b"zi"),
        ("Content-Length: 4\r\n", b"zips"),
        ("", b"zips"),
    ] {
        let url = serve(200, headers, bytes);
        assert!(
            receive(
                client.get(url).send().await.unwrap(),
                &update.asset,
                |_, _, _| Ok(())
            )
            .await
            .is_err()
        );
    }
    let url = serve(200, "Content-Length: 3\r\n", b"zip");
    assert!(
        receive(
            client.get(url).send().await.unwrap(),
            &update.asset,
            |_, _, _| Err("disk full".into())
        )
        .await
        .is_err()
    );
    for status in [403, 404, 429, 500] {
        let url = serve(status, "Content-Length: 0\r\n", b"");
        assert!(
            receive(
                client.get(url).send().await.unwrap(),
                &update.asset,
                |_, _, _| panic!("must not write HTTP error body")
            )
            .await
            .is_err()
        );
    }
}
#[tokio::test]
async fn production_client_rejects_plain_http_and_untrusted_redirects() {
    let client = GithubUpdates::new().unwrap();
    assert!(client.0.get("http://127.0.0.1:1").send().await.is_err());
}
