use super::*;
use proptest::prelude::*;

fn arb_key() -> impl Strategy<Value = Key> {
    (0..Key::ALL.len()).prop_map(|i| Key::ALL[i])
}

fn arb_button() -> impl Strategy<Value = MouseButton> {
    prop_oneof![
        Just(MouseButton::Left),
        Just(MouseButton::Right),
        Just(MouseButton::Middle),
        Just(MouseButton::Back),
        Just(MouseButton::Forward),
    ]
}

fn arb_input() -> impl Strategy<Value = InputEvent> {
    prop_oneof![
        (any::<u16>(), any::<u16>()).prop_map(|(x, y)| InputEvent::MouseMove { x, y }),
        (arb_button(), any::<bool>()).prop_map(|(button, down)| InputEvent::MouseButton { button, down }),
        (-100f32..100f32, -100f32..100f32).prop_map(|(dx, dy)| InputEvent::Scroll { dx, dy }),
        (arb_key(), any::<bool>()).prop_map(|(key, down)| InputEvent::Key { key, down }),
        ".{0,64}".prop_map(|text| InputEvent::Text { text }),
        Just(InputEvent::ReleaseAll),
    ]
}

fn arb_tile() -> impl Strategy<Value = Tile> {
    (
        any::<u32>(),
        any::<u32>(),
        1u16..=64,
        1u16..=64,
        prop_oneof![Just(TileEncoding::Solid), Just(TileEncoding::Zstd), Just(TileEncoding::Jpeg)],
        proptest::collection::vec(any::<u8>(), 0..2048),
    )
        .prop_map(|(x, y, w, h, enc, data)| Tile { x, y, w, h, enc, data })
}

fn arb_client_msg() -> impl Strategy<Value = ClientMsg> {
    prop_oneof![
        (any::<u16>(), ".{0,32}").prop_map(|(proto, client_name)| ClientMsg::Hello {
            proto,
            client_name,
            methods: vec![AuthMethod::Resume, AuthMethod::PublicKey, AuthMethod::Password],
        }),
        ".{0,200}".prop_map(|public_key| ClientMsg::PubkeyQuery { public_key }),
        proptest::collection::vec(any::<u8>(), 0..512).prop_map(|ke1| ClientMsg::PasswordStart { ke1 }),
        (any::<[u8; 16]>(), any::<[u8; 32]>()).prop_map(|(session_id, mac)| ClientMsg::Resume { session_id, mac }),
        any::<u64>().prop_map(|t_us| ClientMsg::Ping { t_us }),
        ".{0,1000}".prop_map(|text| ClientMsg::Clipboard { text }),
        (any::<u32>(), ".{0,100}").prop_map(|(req, path)| ClientMsg::ListDir { req, path }),
    ]
}

fn roundtrip<T>(msg: &T) -> T
where
    T: serde::Serialize + serde::de::DeserializeOwned,
{
    let buf = encode(msg).unwrap();
    let len = u32::from_le_bytes(buf[..4].try_into().unwrap()) as usize;
    assert_eq!(len, buf.len() - 4);
    decode(&buf[4..]).unwrap()
}

proptest! {
    #[test]
    fn input_roundtrip(ev in arb_input()) {
        prop_assert_eq!(roundtrip(&ev), ev);
    }

    #[test]
    fn tile_roundtrip(t in arb_tile()) {
        prop_assert_eq!(roundtrip(&t), t);
    }

    #[test]
    fn client_msg_roundtrip(m in arb_client_msg()) {
        prop_assert_eq!(roundtrip(&m), m);
    }

    #[test]
    fn batched_stream_roundtrip(evs in proptest::collection::vec(arb_input(), 0..32)) {
        let mut buf = Vec::new();
        for ev in &evs {
            encode_into(ev, &mut buf).unwrap();
        }
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let out = rt.block_on(async {
            let mut r: &[u8] = &buf;
            let mut out = Vec::new();
            while let Some(ev) = read_msg::<_, InputEvent>(&mut r, codec::MAX_SMALL_MSG).await.unwrap() {
                out.push(ev);
            }
            out
        });
        prop_assert_eq!(out, evs);
    }
}

#[tokio::test]
async fn clean_eof_is_none() {
    let mut r: &[u8] = &[];
    let m: Option<InputEvent> = read_msg(&mut r, 1024).await.unwrap();
    assert!(m.is_none());
}

#[tokio::test]
async fn truncated_length_prefix() {
    let mut r: &[u8] = &[1, 0];
    let e = read_msg::<_, InputEvent>(&mut r, 1024).await.unwrap_err();
    assert!(matches!(e, CodecError::Truncated), "{e:?}");
}

#[tokio::test]
async fn truncated_payload() {
    let buf = encode(&InputEvent::Text { text: "hello world".into() }).unwrap();
    let mut r: &[u8] = &buf[..buf.len() - 3];
    let e = read_msg::<_, InputEvent>(&mut r, 1024).await.unwrap_err();
    assert!(matches!(e, CodecError::Truncated), "{e:?}");
}

#[tokio::test]
async fn oversized_rejected_before_allocation() {
    let mut buf = (u32::MAX).to_le_bytes().to_vec();
    buf.extend_from_slice(&[0; 16]);
    let mut r: &[u8] = &buf;
    let e = read_msg::<_, InputEvent>(&mut r, 1024).await.unwrap_err();
    assert!(matches!(e, CodecError::TooLarge { .. }), "{e:?}");
}

#[tokio::test]
async fn garbage_payload_is_decode_error() {
    let buf = [3u8, 0, 0, 0, 0xff, 0xff, 0xff];
    let mut r: &[u8] = &buf;
    let e = read_msg::<_, InputEvent>(&mut r, 1024).await.unwrap_err();
    assert!(matches!(e, CodecError::Decode(_)), "{e:?}");
}

#[test]
fn key_names_unique() {
    let mut names: Vec<_> = Key::ALL.iter().map(|k| k.name()).collect();
    names.sort();
    names.dedup();
    assert_eq!(names.len(), Key::ALL.len());
}
