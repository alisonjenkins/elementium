//! Two of a participant's own video tracks must reach two different m-lines.
//!
//! This is the assertion the whole screen-share feature rests on, and it is here rather
//! than in a call test because the failure it guards is invisible from the sending side.
//! With one send mid per media kind, a screen share's frames were written to the camera's
//! mid: str0m accepted them, the publisher's counters advanced, the SFU forwarded a track
//! nobody could decode, and every receiver saw a black rectangle. Nothing anywhere logged
//! an error.
//!
//! Catching that here costs a few milliseconds. Catching it in a call costs a day, because
//! the symptom -- "the far end sees black" -- looks exactly like an encoder fault, an E2EE
//! fault, or a codec negotiation fault, and all three are more famous than this one.

// A failed setup step in a test should stop that test loudly and immediately; the
// workspace's `expect_used` ban is aimed at the shipping paths, not at assertions.
#![allow(clippy::expect_used)]

use elementium_types::MediaTrackKey;
use elementium_webrtc::peer_connection::{
    TransceiverInfo, create_offer, create_peer_connection, write_video,
};
use str0m::media::{Direction, MediaKind};

/// A `sendrecv` video transceiver for one of our own tracks.
fn video_track(cid: &str, key: MediaTrackKey) -> TransceiverInfo {
    TransceiverInfo {
        kind: MediaKind::Video,
        direction: Direction::SendRecv,
        track_id: Some(cid.to_owned()),
        key: Some(key),
    }
}

/// The camera and the screen share must be recorded against different mids.
///
/// Asserted on the recorded send mids rather than on the SDP: a faithful SDP assertion
/// would need a completed offer/answer exchange, and the mid map is the state the write
/// path actually routes on.
#[test]
fn a_camera_and_a_screen_share_get_their_own_send_mids() {
    let mut pc = create_peer_connection("routing".to_owned());

    create_offer(
        &mut pc,
        &[],
        &[
            video_track("sid-video-camera", MediaTrackKey::camera()),
            video_track("sid-video-screen_share", MediaTrackKey::screen_share()),
        ],
    )
    .expect("offer with two video tracks");

    let camera = pc.send_mids.get(&MediaTrackKey::camera()).copied();
    let share = pc.send_mids.get(&MediaTrackKey::screen_share()).copied();

    assert!(camera.is_some(), "the camera must have a send mid");
    assert!(share.is_some(), "the screen share must have a send mid");
    assert_ne!(
        camera, share,
        "the camera and the screen share must not share an m-line; \
         writing one down the other's mid is accepted by the SFU and decodable by nobody"
    );
}

/// Publishing a second video track must not move the first one's mid.
///
/// Starting a screen share mid-call re-offers with both tracks. If the camera's mid moved,
/// its frames would start going to the section the SFU has associated with the share, and
/// the two would swap places for every receiver at once.
#[test]
fn adding_a_screen_share_leaves_the_cameras_mid_alone() {
    let mut pc = create_peer_connection("routing".to_owned());

    let camera = video_track("sid-video-camera", MediaTrackKey::camera());
    create_offer(&mut pc, &[], std::slice::from_ref(&camera)).expect("camera offer");
    let camera_mid = pc
        .send_mids
        .get(&MediaTrackKey::camera())
        .copied()
        .expect("camera mid recorded");

    create_offer(
        &mut pc,
        &[],
        &[
            camera,
            video_track("sid-video-screen_share", MediaTrackKey::screen_share()),
        ],
    )
    .expect("re-offer with the share added");

    assert_eq!(
        pc.send_mids.get(&MediaTrackKey::camera()).copied(),
        Some(camera_mid),
        "the camera must keep its m-line when a share is added alongside it"
    );
}

/// Audio is the same story: the microphone and a shared application's audio are two tracks.
#[test]
fn the_microphone_and_share_audio_get_their_own_send_mids() {
    let mut pc = create_peer_connection("routing".to_owned());

    let audio = |cid: &str, key| TransceiverInfo {
        kind: MediaKind::Audio,
        direction: Direction::SendRecv,
        track_id: Some(cid.to_owned()),
        key: Some(key),
    };

    create_offer(
        &mut pc,
        &[],
        &[
            audio("sid-audio-microphone", MediaTrackKey::microphone()),
            audio(
                "sid-audio-screen_share_audio",
                MediaTrackKey::screen_share_audio(),
            ),
        ],
    )
    .expect("offer with two audio tracks");

    assert_ne!(
        pc.send_mids.get(&MediaTrackKey::microphone()).copied(),
        pc.send_mids
            .get(&MediaTrackKey::screen_share_audio())
            .copied(),
        "the microphone and share audio must not share an m-line"
    );
    assert!(
        pc.send_mids
            .contains_key(&MediaTrackKey::screen_share_audio())
    );
}

/// A write for a track that was never published must fail, not fall back to its kind.
///
/// The tempting fallback -- "no share mid, use the video mid" -- is precisely the bug.
/// It turns a loud, local, immediate error into a silent black picture at every receiver,
/// which is the single most expensive trade in this codebase's history.
#[test]
fn writing_an_unpublished_track_is_refused_rather_than_rerouted() {
    let mut pc = create_peer_connection("routing".to_owned());

    // Only the camera is published.
    create_offer(
        &mut pc,
        &[],
        &[video_track("sid-video-camera", MediaTrackKey::camera())],
    )
    .expect("camera offer");

    let err = write_video(
        &mut pc,
        MediaTrackKey::screen_share(),
        &elementium_types::WireMedia::from_encrypted(vec![0_u8; 16]),
        elementium_codec::VideoCodec::Vp8,
    )
    .expect_err("a share that was never published must not be writable");

    assert!(
        matches!(err, elementium_webrtc::error::WebRtcError::NoMidForTrack(_)),
        "the failure must name the missing track, not a missing writer; got {err:?}"
    );
}

/// A `recvonly` placeholder carries no track of ours and must claim no send mid.
///
/// livekit protocol 17 adds one receive section per remote participant on the same
/// connection we publish on. If one of those claimed a send mid, our media would go down a
/// slot the SFU allocated to somebody else's stream.
#[test]
fn a_receive_only_section_claims_no_send_mid() {
    let mut pc = create_peer_connection("routing".to_owned());

    create_offer(
        &mut pc,
        &[],
        &[
            video_track("sid-video-camera", MediaTrackKey::camera()),
            TransceiverInfo {
                kind: MediaKind::Video,
                direction: Direction::RecvOnly,
                track_id: None,
                key: None,
            },
        ],
    )
    .expect("offer with a receive section");

    assert_eq!(
        pc.send_mids.len(),
        1,
        "only the published track may hold a send mid, got {:?}",
        pc.send_mids
    );
}
