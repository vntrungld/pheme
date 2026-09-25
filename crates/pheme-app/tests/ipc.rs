use std::time::Duration;

use pheme_app::config::Role;
use pheme_app::ipc::{Command, CoreLink, IpcListener, LinkState, Status};

fn sample() -> Status {
    Status {
        role: Role::Server,
        state: LinkState::Connected,
        peer: Some("laptop-win".into()),
        rtt_us: 412,
        locked: false,
        events: 91,
        lost: 0,
        audio_depth_ms: 22,
        audio_lost: 3,
        mic_depth_ms: 0,
        mic_lost: 0,
    }
}

#[tokio::test]
async fn status_and_commands_cross_the_socket() {
    let mut listener = IpcListener::bind().await.unwrap();
    let path = listener.path().to_string();

    let core = tokio::spawn(async move {
        let mut link = CoreLink::connect(&path).await.unwrap();
        link.send_status(&sample()).await.unwrap();
        let c = link.recv_command().await.unwrap();
        assert_eq!(c, Some(Command::Lock));
        // A second status after the command, to prove the connection is still
        // usable in both directions rather than one-shot.
        link.send_status(&sample()).await.unwrap();
    });

    let mut conn = listener.accept().await.unwrap();
    assert_eq!(conn.recv_status().await.unwrap(), Some(sample()));
    conn.send_command(Command::Lock).await.unwrap();
    assert_eq!(conn.recv_status().await.unwrap(), Some(sample()));
    core.await.unwrap();
}

#[tokio::test]
async fn the_core_sees_the_front_end_go_away() {
    // This is the whole lifetime rule: the core exits when the socket closes,
    // so it must be able to tell that it did.
    let mut listener = IpcListener::bind().await.unwrap();
    let path = listener.path().to_string();

    let core = tokio::spawn(async move {
        let mut link = CoreLink::connect(&path).await.unwrap();
        // Blocks until the other end is gone, then reports the close rather
        // than an error.
        link.recv_command().await.unwrap()
    });

    let conn = listener.accept().await.unwrap();
    drop(conn);
    drop(listener);
    let got = tokio::time::timeout(Duration::from_secs(5), core)
        .await
        .expect("the core should notice within five seconds")
        .unwrap();
    assert_eq!(got, None, "a closed connection reads as None, not an error");
}

#[tokio::test]
async fn two_listeners_do_not_collide() {
    // The path carries the process id, so a second front-end in the same
    // session must not fail to bind. Within one process they would, which is
    // why bind() must also tolerate a stale socket file at its path.
    let a = IpcListener::bind().await.unwrap();
    let b = IpcListener::bind().await;
    assert!(b.is_ok(), "a second bind failed: {:?}", b.err());
    drop(a);
}
