//! The operator's console: sign in with the admin token, get a session
//! cookie, and read the same API a bearer would — but only with the custom
//! header, and never without a session.

mod common;

use common::{ADMIN, KEY_ALPHA, StartOptions, batch, device, events, post, start_with};
use std::net::SocketAddr;

fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> (u16, String, Vec<(String, String)>) {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    for (k, v) in headers {
        req.push_str(&format!("{k}: {v}\r\n"));
    }
    req.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    s.write_all(req.as_bytes()).unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).unwrap();
    let text = String::from_utf8_lossy(&raw).to_string();
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    let status: u16 = head.split_whitespace().nth(1).unwrap().parse().unwrap();
    let hs = head
        .lines()
        .skip(1)
        .filter_map(|l| {
            l.split_once(':')
                .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        })
        .collect();
    (status, body.to_string(), hs)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_console_signs_in_once_and_reads_through_the_gate() {
    let mut r = start_with(StartOptions {
        admin_token: Some(ADMIN.into()),
        ..Default::default()
    })
    .await;
    let addr = r.addr;
    let d1 = device("d1");
    let (status, _) = post(addr, Some(KEY_ALPHA), batch(d1, "b1", &events(d1, 3, "s"))).await;
    assert_eq!(status, 200);

    // Not signed in: the console redirects to the login page; the API refuses.
    let (status, _, hs) = tokio::task::spawn_blocking(move || http(addr, "GET", "/admin", &[], ""))
        .await
        .unwrap();
    assert_eq!(status, 303, "a redirect");
    assert!(
        hs.iter()
            .any(|(k, v)| k == "location" && v == "/admin/login"),
        "{hs:?}"
    );
    let (status, _, _) = tokio::task::spawn_blocking(move || {
        http(
            addr,
            "GET",
            "/v1/admin/tenants",
            &[("X-Requested-With", "attemptdb-admin")],
            "",
        )
    })
    .await
    .unwrap();
    assert_eq!(status, 401);

    // Wrong token: 401 and no cookie.
    let (status, body, hs) = tokio::task::spawn_blocking(move || {
        http(
            addr,
            "POST",
            "/admin/login",
            &[("Content-Type", "application/x-www-form-urlencoded")],
            "token=nope",
        )
    })
    .await
    .unwrap();
    assert_eq!(status, 401);
    assert!(body.contains("not the admin token"));
    assert!(!hs.iter().any(|(k, _)| k == "set-cookie"));

    // Right token: a session cookie and a redirect to the console.
    let form = format!("token={ADMIN}");
    let (status, _, hs) = tokio::task::spawn_blocking(move || {
        http(
            addr,
            "POST",
            "/admin/login",
            &[("Content-Type", "application/x-www-form-urlencoded")],
            &form,
        )
    })
    .await
    .unwrap();
    assert_eq!(status, 303);
    let cookie = hs
        .iter()
        .find(|(k, _)| k == "set-cookie")
        .map(|(_, v)| v.split(';').next().unwrap().to_string())
        .expect("session cookie");
    assert!(cookie.starts_with("atdb_admin="));
    let cookie_hdr = cookie.clone();
    let (status, body, _) = tokio::task::spawn_blocking(move || {
        http(addr, "GET", "/admin", &[("Cookie", &cookie_hdr)], "")
    })
    .await
    .unwrap();
    assert_eq!(status, 200);
    assert!(body.contains("admin console"));

    // The API with the cookie: refused without the custom header (a
    // cross-site page could send the cookie, never the header)…
    let c = cookie.clone();
    let (status, _, _) = tokio::task::spawn_blocking(move || {
        http(addr, "GET", "/v1/admin/tenants", &[("Cookie", &c)], "")
    })
    .await
    .unwrap();
    assert_eq!(status, 401);
    // …and served with it, including the operator's read of a tenant.
    let c = cookie.clone();
    let (status, body, _) = tokio::task::spawn_blocking(move || {
        http(
            addr,
            "GET",
            "/v1/admin/tenants",
            &[("Cookie", &c), ("X-Requested-With", "attemptdb-admin")],
            "",
        )
    })
    .await
    .unwrap();
    assert_eq!(status, 200, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    let alpha = v["tenants"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["tenant"] == "alpha")
        .expect("alpha listed");
    assert_eq!(alpha["devices"], 1);
    assert!(alpha["open"].as_bool().unwrap());
    let c = cookie.clone();
    let (status, body, _) = tokio::task::spawn_blocking(move || {
        http(
            addr,
            "GET",
            "/v1/status",
            &[
                ("Cookie", &c),
                ("X-Requested-With", "attemptdb-admin"),
                ("X-AttemptDB-Tenant", "alpha"),
            ],
            "",
        )
    })
    .await
    .unwrap();
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("\"events\":3"));

    // Sign out: the cookie is dead.
    let c = cookie.clone();
    let (status, _, _) = tokio::task::spawn_blocking(move || {
        http(addr, "POST", "/admin/logout", &[("Cookie", &c)], "")
    })
    .await
    .unwrap();
    assert_eq!(status, 303);
    let c = cookie.clone();
    let (status, _, _) = tokio::task::spawn_blocking(move || {
        http(
            addr,
            "GET",
            "/v1/admin/tenants",
            &[("Cookie", &c), ("X-Requested-With", "attemptdb-admin")],
            "",
        )
    })
    .await
    .unwrap();
    assert_eq!(status, 401);
    r.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_an_admin_token_the_console_does_not_exist() {
    let mut r = start_with(StartOptions::default()).await;
    let addr = r.addr;
    let (status, _, _) = tokio::task::spawn_blocking(move || http(addr, "GET", "/admin", &[], ""))
        .await
        .unwrap();
    assert_eq!(status, 404);
    let (status, _, _) =
        tokio::task::spawn_blocking(move || http(addr, "GET", "/admin/login", &[], ""))
            .await
            .unwrap();
    assert_eq!(status, 404);
    r.stop().await;
}
