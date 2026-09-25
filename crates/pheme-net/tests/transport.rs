use std::net::SocketAddr;
use std::time::{Duration, Instant};

use pheme_net::{Endpoint, Identity, Incoming, NetError, TrustStore};
use pheme_proto::{AudioParams, Msg, Os, PROTOCOL_VERSION};

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
        version: PROTOCOL_VERSION,
        name: name.into(),
        os: Os::Linux,
        screens: vec![],
        audio: AudioParams::DEFAULT,
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

// Either the connect itself fails, or (since TLS 1.3 lets the client finish its handshake
// before the server has verified the client certificate) it briefly succeeds and the server
// closes it right after: both are acceptable evidence of rejection.
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
    match res {
        Err(_) => {}
        Ok(peer) => {
            // TLS 1.3 lets the client finish its handshake before the server verifies the
            // client certificate; the rejection then arrives as a prompt close.
            let reason = tokio::time::timeout(Duration::from_secs(2), peer.closed())
                .await
                .expect("server must close an untrusted client promptly");
            assert!(
                !matches!(reason, pheme_net::CloseReason::LocallyClosed),
                "{reason:?}"
            );
        }
    }
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
    let err = client.connect(addr).await.unwrap_err();
    assert!(matches!(err, NetError::Untrusted(_)), "{err:?}");
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

#[tokio::test]
async fn dropping_peer_closes_the_connection() {
    let s = side("server");
    let c = side("client");
    trust_each_other(&s, &c);
    let server = Endpoint::server("127.0.0.1:0".parse().unwrap(), &s.id, s.trust.clone()).unwrap();
    let addr = server.local_addr().unwrap();
    let client = Endpoint::client(&c.id, c.trust.clone()).unwrap();

    let server_task = tokio::spawn(async move {
        let Incoming::Peer(peer) = server.accept().await.unwrap() else {
            panic!("expected peer")
        };
        peer
    });

    let peer = client.connect(addr).await.unwrap();
    peer.sender().send_control(&hello("client")).await.unwrap();
    let server_peer = server_task.await.unwrap();

    // Drop without an explicit close(); `Peer`'s `Drop` impl must close the connection itself.
    drop(peer);

    let reason = tokio::time::timeout(Duration::from_secs(2), server_peer.closed())
        .await
        .expect("server-side peer must observe the drop-triggered close promptly");
    assert!(
        !matches!(reason, pheme_net::CloseReason::LocallyClosed),
        "{reason:?}"
    );
}

/// Audio must not be able to delay input. The two travel on the same QUIC connection and
/// used to share one bounded channel, so a receiver that stalled for a moment would find
/// a burst of audio queued ahead of the next mouse movement.
#[tokio::test]
async fn audio_cannot_queue_ahead_of_input() {
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
        let mut input = peer.take_incoming();
        let mut audio = peer.take_audio();
        // The control stream is ordered and reliable, so the Key sent last arrives; what
        // matters is that it is not queued behind the audio, which goes to its own
        // channel entirely.
        match input.recv().await.unwrap() {
            Msg::Key { seq: 42, .. } => {}
            other => panic!("input channel delivered {other:?}"),
        }
        let mut frames = 0;
        while let Ok(Some(m)) = tokio::time::timeout(Duration::from_secs(2), audio.recv()).await {
            assert!(
                matches!(m, Msg::Audio { .. }),
                "the audio channel carried {m:?}"
            );
            frames += 1;
            if frames == 8 {
                break;
            }
        }
        assert_eq!(frames, 8, "every audio frame reached the audio channel");
        peer.close("done");
    });

    let peer = client.connect(addr).await.unwrap();
    for seq in 0..8 {
        peer.sender().send_datagram(&Msg::Audio {
            stream: pheme_proto::AudioStream::Mic,
            seq,
            ts_us: u64::from(seq) * 5_000,
            samples: vec![0u8; 960],
        });
    }
    peer.sender()
        .send_control(&Msg::Key {
            seq: 42,
            code: pheme_proto::KeyCode(0x04),
            down: true,
        })
        .await
        .unwrap();
    server_task.await.unwrap();
}

/// Spec §2.5: a full audio channel drops the frame being delivered **and counts it**.
///
/// This is the one audio loss nothing downstream can attribute — the frame never reaches
/// the jitter buffer, so its own `dropped` and `overflows` stay clean while the listener
/// hears a gap. The frames go on the control stream because it is reliable and ordered:
/// every one of them is certain to arrive, so a missing count is the counter's fault and
/// not the network's.
#[tokio::test]
async fn a_full_audio_channel_counts_the_frames_it_drops() {
    let s = side("server");
    let c = side("client");
    trust_each_other(&s, &c);
    let server = Endpoint::server("127.0.0.1:0".parse().unwrap(), &s.id, s.trust.clone()).unwrap();
    let addr: SocketAddr = server.local_addr().unwrap();
    let client = Endpoint::client(&c.id, c.trust.clone()).unwrap();

    let server_task = tokio::spawn(async move {
        let Incoming::Peer(peer) = server.accept().await.unwrap() else {
            panic!("expected peer")
        };
        peer
    });

    let peer = client.connect(addr).await.unwrap();
    // Nothing ever calls `take_audio` on the server side, so the 32-frame audio channel
    // fills and stays full.
    for seq in 0..128 {
        peer.sender()
            .send_control(&Msg::Audio {
                stream: pheme_proto::AudioStream::Mic,
                seq,
                ts_us: u64::from(seq) * 5_000,
                samples: vec![0u8; 960],
            })
            .await
            .unwrap();
    }
    let server_peer = server_task.await.unwrap();

    let t = Instant::now();
    while server_peer.audio_dropped() < 64 && t.elapsed() < Duration::from_secs(5) {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        server_peer.audio_dropped() >= 64,
        "128 frames into a 32-deep channel counted only {} drops",
        server_peer.audio_dropped()
    );
    peer.close("done");
}

#[tokio::test]
async fn a_clipboard_message_crosses_on_its_own_stream() {
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
        let mut rx = peer.take_incoming();
        let mut clip = peer.take_clipboard();
        assert_eq!(rx.recv().await.unwrap(), hello("client"));
        let m = clip.recv().await.unwrap();
        assert_eq!(
            m,
            Msg::Clipboard {
                mime: pheme_proto::CLIP_MIME.to_string(),
                data: b"hello from the client".to_vec(),
            }
        );
        // The control stream still works afterwards.
        peer.sender().send_control(&Msg::Pong(9)).await.unwrap();
        // Returned, not dropped here: `Peer`'s `Drop` closes the connection, and
        // quinn's own docs warn that a close can discard data already accepted
        // for send but not yet delivered to the peer's application. Keeping
        // `peer` alive until the caller has confirmed the `Pong` arrived avoids
        // racing the reply against the connection teardown.
        peer
    });

    let mut peer = client.connect(addr).await.unwrap();
    let mut rx = peer.take_incoming();
    peer.sender().send_control(&hello("client")).await.unwrap();
    peer.sender()
        .send_clipboard(&Msg::Clipboard {
            mime: pheme_proto::CLIP_MIME.to_string(),
            data: b"hello from the client".to_vec(),
        })
        .await
        .unwrap();
    assert_eq!(rx.recv().await.unwrap(), Msg::Pong(9));
    server_task.await.unwrap();
}

#[tokio::test]
async fn an_oversized_clipboard_stream_is_dropped_and_the_connection_lives() {
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
        let mut rx = peer.take_incoming();
        let mut clip = peer.take_clipboard();
        assert_eq!(rx.recv().await.unwrap(), hello("client"));
        // The oversized stream produces nothing, and the small one that follows
        // still arrives: the reader rejects one stream, not the connection.
        let m = clip.recv().await.unwrap();
        assert_eq!(
            m,
            Msg::Clipboard {
                mime: pheme_proto::CLIP_MIME.to_string(),
                data: b"small".to_vec(),
            }
        );
        peer.sender().send_control(&Msg::Pong(9)).await.unwrap();
        // Returned, not dropped here: `Peer`'s `Drop` closes the connection, and
        // quinn's own docs warn that a close can discard data already accepted
        // for send but not yet delivered to the peer's application. Keeping
        // `peer` alive until the caller has confirmed the `Pong` arrived avoids
        // racing the reply against the connection teardown.
        peer
    });

    let mut peer = client.connect(addr).await.unwrap();
    let mut rx = peer.take_incoming();
    peer.sender().send_control(&hello("client")).await.unwrap();
    // The receiver rejects this stream and may signal STOP_SENDING, which can make
    // this write fail on the sender's own side — that is correct behaviour, not a
    // test failure, so the result is not unwrapped.
    let _ = peer
        .sender()
        .send_clipboard(&Msg::Clipboard {
            mime: pheme_proto::CLIP_MIME.to_string(),
            data: vec![b'x'; pheme_proto::MAX_CLIP_BYTES + pheme_proto::CLIP_FRAME_SLACK + 1],
        })
        .await;
    peer.sender()
        .send_clipboard(&Msg::Clipboard {
            mime: pheme_proto::CLIP_MIME.to_string(),
            data: b"small".to_vec(),
        })
        .await
        .unwrap();
    assert_eq!(rx.recv().await.unwrap(), Msg::Pong(9));
    server_task.await.unwrap();
}

#[tokio::test]
async fn a_unidirectional_stream_carrying_something_else_is_ignored() {
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
        let mut rx = peer.take_incoming();
        let mut clip = peer.take_clipboard();
        assert_eq!(rx.recv().await.unwrap(), hello("client"));
        // A `Ping` on a clipboard stream must not reach the clipboard channel,
        // and must not be mistaken for input either.
        let m = clip.recv().await.unwrap();
        assert!(matches!(m, Msg::Clipboard { .. }), "got {m:?}");
        peer.sender().send_control(&Msg::Pong(9)).await.unwrap();
        // Returned, not dropped here: `Peer`'s `Drop` closes the connection, and
        // quinn's own docs warn that a close can discard data already accepted
        // for send but not yet delivered to the peer's application. Keeping
        // `peer` alive until the caller has confirmed the `Pong` arrived avoids
        // racing the reply against the connection teardown.
        peer
    });

    let mut peer = client.connect(addr).await.unwrap();
    let mut rx = peer.take_incoming();
    peer.sender().send_control(&hello("client")).await.unwrap();
    peer.sender().send_clipboard(&Msg::Ping(1)).await.unwrap();
    peer.sender()
        .send_clipboard(&Msg::Clipboard {
            mime: pheme_proto::CLIP_MIME.to_string(),
            data: b"after the ping".to_vec(),
        })
        .await
        .unwrap();
    assert_eq!(rx.recv().await.unwrap(), Msg::Pong(9));
    server_task.await.unwrap();
}
