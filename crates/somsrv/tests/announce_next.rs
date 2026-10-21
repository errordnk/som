//! A player that already knows a track and the one after it (a radio batch) says
//! so with `AnnounceTrack`: the daemon shows the track's tags and cover at once
//! and passes the next track on in `NowPlayingInfo::next`, so Som can have its
//! cover ready before it starts.

use somsrv::protocol::{ConnectionKind, HandshakeInfo, NextTrack, NowPlayingInfo, SrvRequest, SrvResponse};
use std::time::{Duration, Instant};

fn track(title: &str, cover: &str) -> NextTrack {
    NextTrack {
        artist: "Portishead".into(),
        title: title.into(),
        album: "Dummy".into(),
        year: Some(1994),
        duration_ms: 300_000,
        cover_url: Some(cover.into()),
    }
}

#[test]
fn the_tags_and_the_next_cover_of_an_announced_track_reach_a_subscriber() {
    let (done, finished) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let connection = somsrv::daemon::connect_or_spawn(std::path::Path::new(env!("CARGO_BIN_EXE_somsrv"))).expect("reaching the daemon");
        ConnectionKind::Srv.write_to(&connection).unwrap();
        connection.write_message(&serde_json::to_vec(&SrvRequest::Handshake(HandshakeInfo::current())).unwrap()).unwrap();
        connection.read_message().unwrap();

        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64;
        let (session_id, file_id) = (((now & 0xFF_FFFF) as u32).max(1), (((now >> 24) & 0xFF_FFFF) as u32).max(1));
        connection.write_message(&serde_json::to_vec(&SrvRequest::SubscribeProgress { session_id, file_id }).unwrap()).unwrap();
        // Requests on one connection are handled in order: once this is answered the subscription is in.
        connection.write_message(&serde_json::to_vec(&SrvRequest::ListSessions { client_id: None }).unwrap()).unwrap();
        loop {
            let message = connection.read_message().unwrap();
            if matches!(serde_json::from_slice::<SrvResponse>(&message), Ok(SrvResponse::Sessions(_))) {
                break;
            }
        }

        let announce = SrvRequest::AnnounceTrack {
            session_id,
            file_id,
            title: Some("Portishead - Roads".into()),
            index: 3,
            len: 10,
            target: "https://music.yandex.ru/album/3193/track/38526".into(),
            tags: None,
            looping: false,
            current: Some(track("Roads", "https://avatars.yandex.net/a/1000x1000")),
            next: Some(track("Glory Box", "https://avatars.yandex.net/b/1000x1000")),
        };
        connection.write_message(&serde_json::to_vec(&announce).unwrap()).unwrap();

        let deadline = Instant::now() + Duration::from_secs(20);
        let mut shown: Option<NowPlayingInfo> = None;
        while Instant::now() < deadline && shown.is_none() {
            let message = connection.read_message().unwrap();
            if let Ok(SrvResponse::NowPlaying(info)) = serde_json::from_slice::<SrvResponse>(&message)
                && info.title == "Roads"
            {
                shown = Some(info);
            }
        }
        done.send(shown).ok();
    });
    let shown = finished.recv_timeout(Duration::from_secs(60)).expect("no result in time").expect("the announced track was never shown");
    assert_eq!((shown.artist.as_str(), shown.album.as_str(), shown.year), ("Portishead", "Dummy", Some(1994)));
    assert_eq!(shown.cover_url.as_deref(), Some("https://avatars.yandex.net/a/1000x1000"), "the cover of the track itself");
    assert!(!shown.cover_lookup_pending, "nothing is looked up: the cover is known");
    let next = shown.next.expect("the next track is passed on");
    assert_eq!(next.title, "Glory Box");
    assert_eq!(next.cover_url.as_deref(), Some("https://avatars.yandex.net/b/1000x1000"), "its cover, for Som to fetch ahead");
}
