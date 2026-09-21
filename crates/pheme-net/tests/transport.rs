use std::net::SocketAddr;
use std::time::{Duration, Instant};

use pheme_net::{Endpoint, Identity, Incoming, TrustStore};
use pheme_proto::{Msg, Os};

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

fn trust_each_other(a: &Side, b: &Side) {
    a.trust.write().unwrap().add(&b.id.name, &b.id.fingerprint);
    b.trust.write().unwrap().add(&a.id.name, &a.id.fingerprint);
}

fn hello(name: &str) -> Msg {
    Msg::Hello {
        version: 1,
        name: name.into(),
        os: Os::Linux,
        screens: vec![],
    }
}

#[tokio::test]
async fn control_and_datagram_roundtrip() {
    let s = side("server");
    let c = side("client");
    trust_each_other(&s, &c);
    let server = Endpoint::server("127.0.0.1:0".parse().unwrap(), &s.id, s.trust.clone()).unwrap();
    let addr: SocketAddr = server.local_addr().unwrap();
    let client = Endpoint::client(&c.id, c.trust.clone()).unwrap();

    let server_task = tokio::spawn(async move {
        let Incoming::Peer(mut peer) = server.accept().await.unwrap() else {
            panic!("expected peer")
        };
        assert_eq!(peer.remote_name(), "client");
        let mut rx = peer.take_incoming();
        assert_eq!(rx.recv().await.unwrap(), hello("client"));
        peer.sender().send_control(&Msg::Pong(1)).await.unwrap();
        // wait for at least one datagram (loss on loopback is not expected)
        match rx.recv().await.unwrap() {
            Msg::MouseMove { dx: 3, dy: -3, .. } => {}
            other => panic!("unexpected {other:?}"),
        }
        peer.close("done");
    });

    let mut peer = client.connect(addr).await.unwrap();
    assert_eq!(peer.remote_name(), "server");
    peer.sender().send_control(&hello("client")).await.unwrap();
    let mut rx = peer.take_incoming();
    assert_eq!(rx.recv().await.unwrap(), Msg::Pong(1));
    peer.sender().send_datagram(&Msg::MouseMove {
        seq: 1,
        dx: 3,
        dy: -3,
    });
    server_task.await.unwrap();
    let t = Instant::now();
    let reason = peer.closed().await;
    assert!(
        matches!(reason, pheme_net::CloseReason::ApplicationClosed(ref r) if r == "done"),
        "{reason:?}"
    );
    assert!(t.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn untrusted_client_is_rejected() {
    let s = side("server");
    let c = side("client");
    // only the client trusts the server; the server does not know the client
    c.trust.write().unwrap().add(&s.id.name, &s.id.fingerprint);
    let server = Endpoint::server("127.0.0.1:0".parse().unwrap(), &s.id, s.trust.clone()).unwrap();
    let addr = server.local_addr().unwrap();
    let client = Endpoint::client(&c.id, c.trust.clone()).unwrap();
    let accept = tokio::spawn(async move { server.accept().await });
    let res = client.connect(addr).await;
    assert!(res.is_err(), "connect must fail: {res:?}");
    accept.abort();
}

#[tokio::test]
async fn untrusted_server_is_rejected_by_client() {
    let s = side("server");
    let c = side("client");
    s.trust.write().unwrap().add(&c.id.name, &c.id.fingerprint);
    let server = Endpoint::server("127.0.0.1:0".parse().unwrap(), &s.id, s.trust.clone()).unwrap();
    let addr = server.local_addr().unwrap();
    let client = Endpoint::client(&c.id, c.trust.clone()).unwrap();
    let accept = tokio::spawn(async move { server.accept().await });
    assert!(client.connect(addr).await.is_err());
    accept.abort();
}

#[tokio::test]
async fn server_shutdown_closes_peer_quickly() {
    let s = side("server");
    let c = side("client");
    trust_each_other(&s, &c);
    let server = Endpoint::server("127.0.0.1:0".parse().unwrap(), &s.id, s.trust.clone()).unwrap();
    let addr = server.local_addr().unwrap();
    let client = Endpoint::client(&c.id, c.trust.clone()).unwrap();
    let accept = tokio::spawn(async move {
        let Incoming::Peer(mut p) = server.accept().await.unwrap() else {
            panic!()
        };
        let mut rx = p.take_incoming();
        rx.recv().await;
        server.close();
    });
    let peer = client.connect(addr).await.unwrap();
    peer.sender().send_control(&hello("client")).await.unwrap();
    let t = Instant::now();
    let _ = peer.closed().await;
    assert!(t.elapsed() < Duration::from_secs(6));
    accept.await.unwrap();
}
