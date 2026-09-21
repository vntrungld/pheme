use std::time::Duration;

use pheme_net::pairing::{client_pair, generate_code, run_server_pairing};
use pheme_net::{Endpoint, Identity, TrustStore};

struct Side {
    id: Identity,
    trust: pheme_net::SharedTrust,
    _dir: tempfile::TempDir,
}

fn side(name: &str) -> Side {
    let dir = tempfile::tempdir().unwrap();
    let id = Identity::load_or_create(dir.path(), name).unwrap();
    let trust = TrustStore::load(dir.path()).unwrap().shared();
    Side {
        id,
        trust,
        _dir: dir,
    }
}

#[test]
fn code_is_six_digits() {
    for _ in 0..100 {
        let c = generate_code();
        assert_eq!(c.len(), 6);
        assert!(c.bytes().all(|b| b.is_ascii_digit()));
    }
}

#[tokio::test]
async fn correct_code_pairs_both_sides() {
    let s = side("server");
    let c = side("client");
    let server = Endpoint::server("127.0.0.1:0".parse().unwrap(), &s.id, s.trust.clone()).unwrap();
    let addr = server.local_addr().unwrap();
    let code = "123456".to_string();
    let s_fp = s.id.fingerprint.clone();
    let (s_id, s_trust) = (s.id, s.trust.clone());
    let server_task = tokio::spawn(async move {
        run_server_pairing(&server, &code, &s_id, s_trust, 3, Duration::from_secs(10)).await
    });
    let client = Endpoint::pairing_client(&c.id, c.trust.clone()).unwrap();
    let server_name = client_pair(&client, addr, "123456", &c.id, c.trust.clone())
        .await
        .unwrap();
    assert_eq!(server_name, "server");
    let client_name = server_task.await.unwrap().unwrap();
    assert_eq!(client_name, "client");
    assert!(s.trust.read().unwrap().is_trusted(&c.id.fingerprint));
    assert!(c.trust.read().unwrap().is_trusted(&s_fp));
    assert_eq!(
        c.trust.read().unwrap().name_of(&s_fp).as_deref(),
        Some("server")
    );
    assert!(s._dir.path().join("trusted.toml").exists(), "saved to disk");
}

#[tokio::test]
async fn wrong_code_fails_and_trusts_nothing() {
    let s = side("server");
    let c = side("client");
    let server = Endpoint::server("127.0.0.1:0".parse().unwrap(), &s.id, s.trust.clone()).unwrap();
    let addr = server.local_addr().unwrap();
    let (s_id, s_trust) = (s.id, s.trust.clone());
    let server_task = tokio::spawn(async move {
        run_server_pairing(
            &server,
            "123456",
            &s_id,
            s_trust,
            1,
            Duration::from_secs(10),
        )
        .await
    });
    let client = Endpoint::pairing_client(&c.id, c.trust.clone()).unwrap();
    let res = client_pair(&client, addr, "654321", &c.id, c.trust.clone()).await;
    assert!(res.is_err());
    assert!(
        server_task.await.unwrap().is_err(),
        "server gives up after max_failures"
    );
    assert!(s.trust.read().unwrap().peers().is_empty());
    assert!(c.trust.read().unwrap().peers().is_empty());
}

#[tokio::test]
async fn pairing_mode_ends_after_one_success() {
    let s = side("server");
    let c = side("client");
    let server = Endpoint::server("127.0.0.1:0".parse().unwrap(), &s.id, s.trust.clone()).unwrap();
    let addr = server.local_addr().unwrap();
    let code = "123456".to_string();
    let (s_id, s_trust) = (s.id, s.trust.clone());
    let server_task = tokio::spawn(async move {
        let r =
            run_server_pairing(&server, &code, &s_id, s_trust, 3, Duration::from_secs(10)).await;
        (server, r)
    });
    let client = Endpoint::pairing_client(&c.id, c.trust.clone()).unwrap();
    let server_name = client_pair(&client, addr, "123456", &c.id, c.trust.clone())
        .await
        .unwrap();
    assert_eq!(server_name, "server");
    let (server, client_name) = server_task.await.unwrap();
    assert_eq!(client_name.unwrap(), "client");

    let intruder = side("intruder");
    let accept = tokio::spawn(async move { server.accept().await });
    let intruder_client = Endpoint::pairing_client(&intruder.id, intruder.trust.clone()).unwrap();
    let res = tokio::time::timeout(
        Duration::from_secs(5),
        client_pair(
            &intruder_client,
            addr,
            "123456",
            &intruder.id,
            intruder.trust.clone(),
        ),
    )
    .await;
    assert!(matches!(res, Ok(Err(_))), "{res:?}");
    accept.abort();
}

#[tokio::test]
async fn pairing_is_refused_when_not_in_pairing_mode() {
    let s = side("server");
    let c = side("client");
    let server = Endpoint::server("127.0.0.1:0".parse().unwrap(), &s.id, s.trust.clone()).unwrap();
    let addr = server.local_addr().unwrap();
    let accept = tokio::spawn(async move { server.accept().await });
    let client = Endpoint::pairing_client(&c.id, c.trust.clone()).unwrap();
    let res = tokio::time::timeout(
        Duration::from_secs(5),
        client_pair(&client, addr, "123456", &c.id, c.trust.clone()),
    )
    .await;
    assert!(matches!(res, Ok(Err(_))), "{res:?}");
    accept.abort();
}
