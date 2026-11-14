use super::*;
use crate::content::Content;
use tempfile::{TempDir, tempdir};

const PNG: &[u8] = include_bytes!("tests/fixtures/red.png");
const JPEG_HEADER: &[u8] = &[0xff, 0xd8, 0xff, 0xe0, 1, 2, 3];
const GIF_HEADER: &[u8] = b"GIF89a\x01\x00\x01\x00";
const WEBP_HEADER: &[u8] = b"RIFF\x24\x00\x00\x00WEBPVP8 ";

fn workspace(dir: &TempDir) -> PathBuf {
    dir.path().canonicalize().unwrap()
}

fn write(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, bytes).unwrap();
    path
}

fn asset(dir: &TempDir) -> ImageAsset {
    snapshot_bytes(&workspace(dir), PNG).unwrap()
}

#[cfg(target_os = "linux")]
#[test]
fn directory_sync_reopens_an_o_path_capability() {
    use std::os::unix::fs::OpenOptionsExt;

    let dir = tempdir().unwrap();
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_DIRECTORY)
        .open(dir.path())
        .unwrap();
    assert_eq!(
        file.try_clone()
            .unwrap()
            .sync_all()
            .unwrap_err()
            .raw_os_error(),
        Some(libc::EBADF),
        "cloning an O_PATH descriptor does not make it synchronizable"
    );
    let capability = cap_std::fs::Dir::from_std_file(file);
    sync_directory(&capability).expect("directory sync must reopen the O_PATH capability");
}

#[cfg(unix)]
#[test]
fn directory_sync_stays_with_the_pinned_directory_after_a_path_is_replaced() {
    let dir = tempdir().unwrap();
    let original = workspace(&dir).join("original");
    let moved = workspace(&dir).join("moved");
    std::fs::create_dir(&original).unwrap();
    let capability = crate::pinned_dir::open_canonical(&original).unwrap();
    std::fs::rename(&original, &moved).unwrap();
    std::os::unix::fs::symlink(workspace(&dir).join("missing"), &original).unwrap();

    sync_directory(&capability).expect("sync must use the capability, not its replaced path");
    assert!(moved.is_dir());
    assert!(std::fs::read_link(original).unwrap().ends_with("missing"));
}

fn with_image(mut message: Message, image: ImageRef) -> Message {
    message.content.blocks.push(ContentBlock::Image { image });
    message
}

#[test]
fn headers_identify_supported_formats_without_trusting_extensions() {
    for (bytes, format) in [
        (PNG, ImageFormat::Png),
        (JPEG_HEADER, ImageFormat::Jpeg),
        (GIF_HEADER, ImageFormat::Gif),
        (WEBP_HEADER, ImageFormat::WebP),
    ] {
        assert_eq!(ImageFormat::sniff(bytes), Some(format));
    }
    for bytes in [b"{}".as_slice(), b"", b"\x89PNG", b"RIFF\x24\0\0\0WAVEfmt "] {
        assert_eq!(ImageFormat::sniff(bytes), None);
    }
    let dir = tempdir().unwrap();
    assert_eq!(
        inspect(&write(dir.path(), "pretend.png", b"{}")),
        Err(ImageError::UnsupportedFormat)
    );
}

#[test]
fn png_fixture_is_a_complete_image_not_only_a_magic_prefix() {
    assert!(PNG.len() > 60);
    assert!(PNG.windows(4).any(|bytes| bytes == b"IHDR"));
    assert!(PNG.windows(4).any(|bytes| bytes == b"IDAT"));
    assert!(PNG.windows(4).any(|bytes| bytes == b"IEND"));
    assert_eq!(&PNG[16..24], &[0, 0, 0, 1, 0, 0, 0, 1]);
}

#[test]
fn draft_inspection_is_bounded_and_reports_actual_format() {
    let dir = tempdir().unwrap();
    let path = write(dir.path(), "image.bin", PNG);
    let attachment = inspect(&path).unwrap();
    assert_eq!(attachment.path, path);
    assert_eq!(attachment.format, ImageFormat::Png);
    assert_eq!(attachment.bytes, PNG.len() as u64);
    assert_eq!(
        inspect(&dir.path().join("missing.png")),
        Err(ImageError::Missing)
    );
    assert_eq!(inspect(dir.path()), Err(ImageError::NotAFile));
    let large = std::fs::File::create(dir.path().join("huge.png")).unwrap();
    large.set_len(MAX_IMAGE_BYTES + 1).unwrap();
    assert_eq!(
        inspect(&dir.path().join("huge.png")),
        Err(ImageError::TooLarge {
            bytes: MAX_IMAGE_BYTES + 1
        })
    );
}

#[test]
fn same_bytes_have_one_stable_asset_and_round_trip_through_json() {
    let dir = tempdir().unwrap();
    let first = asset(&dir);
    let again = snapshot_bytes(&workspace(&dir), PNG).unwrap();
    assert_eq!(first, again);
    assert_eq!(first.read().unwrap(), PNG);
    assert_eq!(
        std::fs::read_dir(workspace(&dir).join(IMAGES_DIR))
            .unwrap()
            .count(),
        1
    );
    assert_eq!(first.digest.len(), 64);
    assert_eq!(
        serde_json::from_str::<ImageAsset>(&serde_json::to_string(&first).unwrap()).unwrap(),
        first
    );
    let url = first.data_url().unwrap();
    assert_eq!(
        url,
        format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(PNG)
        )
    );
}

#[test]
fn admission_rejects_size_and_format_before_creating_storage() {
    let dir = tempdir().unwrap();
    assert!(matches!(
        snapshot_bytes(&workspace(&dir), b"{}"),
        Err(StoreError::UnsupportedFormat)
    ));
    assert!(!workspace(&dir).join(IMAGES_DIR).exists());
    let mut bytes = vec![0; MAX_IMAGE_BYTES as usize + 1];
    bytes[..8].copy_from_slice(&PNG[..8]);
    assert!(matches!(
        snapshot_bytes(&workspace(&dir), &bytes),
        Err(StoreError::TooLarge { .. })
    ));
    assert!(!workspace(&dir).join(IMAGES_DIR).exists());
}

#[test]
fn source_changes_do_not_rewrite_an_observation() {
    let dir = tempdir().unwrap();
    let source = write(dir.path(), "source.png", PNG);
    let (assets, notes) = snapshot_paths(&workspace(&dir), std::slice::from_ref(&source));
    assert!(notes.is_empty());
    std::fs::write(&source, b"replaced source").unwrap();
    std::fs::remove_file(source).unwrap();
    assert_eq!(assets[0].read().unwrap(), PNG);
}

#[test]
fn asset_replay_checks_digest_even_when_length_and_header_match() {
    let dir = tempdir().unwrap();
    let image = asset(&dir);
    let mut bytes = PNG.to_vec();
    *bytes.last_mut().unwrap() ^= 1;
    std::fs::write(image.path(), &bytes).unwrap();
    assert!(matches!(image.read(), Err(ImageError::Unreadable(_))));
    assert!(snapshot_bytes(&workspace(&dir), PNG).is_err());
    assert_eq!(std::fs::read(image.path()).unwrap(), bytes);
}

#[test]
fn asset_identity_cannot_escape_its_store() {
    let dir = tempdir().unwrap();
    let mut image = asset(&dir);
    image.digest = "../escape".into();
    assert!(matches!(image.read(), Err(ImageError::Unreadable(_))));
    assert!(!workspace(&dir).join("escape.png").exists());
}

#[test]
fn metadata_mismatch_and_missing_assets_are_explicit() {
    let dir = tempdir().unwrap();
    let mut image = asset(&dir);
    image.bytes += 1;
    assert!(matches!(image.read(), Err(ImageError::Unreadable(_))));
    image.bytes -= 1;
    std::fs::remove_file(image.path()).unwrap();
    assert_eq!(image.read(), Err(ImageError::Missing));
}

#[test]
fn asset_reads_and_publication_refuse_leaf_symlinks() {
    for existing in [false, true] {
        let dir = tempdir().unwrap();
        let outside = tempdir().unwrap();
        let image = asset(&dir);
        let target = outside.path().join("external.png");
        if existing {
            std::fs::write(&target, PNG).unwrap();
        }
        std::fs::remove_file(image.path()).unwrap();
        if !crate::test_symlinks::symlink_file_for_test(&target, &image.path()) {
            return;
        }
        assert!(image.read().is_err());
        assert!(snapshot_bytes(&workspace(&dir), PNG).is_err());
        assert_eq!(target.exists(), existing);
        if existing {
            assert_eq!(std::fs::read(target).unwrap(), PNG);
        }
    }
}

#[cfg(unix)]
#[test]
fn stored_observation_refuses_replaced_parent_directories() {
    let dir = tempdir().unwrap();
    let outside = tempdir().unwrap();
    let image = asset(&dir);
    let images = workspace(&dir).join(IMAGES_DIR);
    std::fs::rename(&images, workspace(&dir).join("parked-images")).unwrap();
    std::os::unix::fs::symlink(outside.path(), &images).unwrap();
    assert!(image.read().is_err());
    assert!(snapshot_bytes(&workspace(&dir), PNG).is_err());
    assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
}

#[test]
fn concurrent_admission_publishes_complete_identical_objects() {
    let dir = tempdir().unwrap();
    let root = workspace(&dir);
    let threads = (0..8)
        .map(|_| {
            let root = root.clone();
            std::thread::spawn(move || snapshot_bytes(&root, PNG).unwrap())
        })
        .collect::<Vec<_>>();
    for thread in threads {
        assert_eq!(thread.join().unwrap().read().unwrap(), PNG);
    }
    assert_eq!(std::fs::read_dir(root.join(IMAGES_DIR)).unwrap().count(), 1);
}

#[test]
fn explicit_user_admission_keeps_good_images_and_names_failures() {
    let dir = tempdir().unwrap();
    let good = write(dir.path(), "image.png", PNG);
    let invalid = write(dir.path(), "bad.png", b"not an image");
    let missing = dir.path().join("gone.png");
    let (images, notes) = snapshot_paths(&workspace(&dir), &[good, invalid, missing]);
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].read().unwrap(), PNG);
    assert_eq!(notes.len(), 2);
    assert!(notes[0].contains("bad.png") && notes[0].contains("unsupported image format"));
    assert!(notes[1].contains("gone.png") && notes[1].contains("image unavailable"));
}

#[test]
fn admission_count_is_a_product_limit_and_reports_omission() {
    let dir = tempdir().unwrap();
    let source = write(dir.path(), "same.png", PNG);
    let (images, notes) =
        snapshot_paths(&workspace(&dir), &vec![source; MAX_IMAGES_PER_MESSAGE + 1]);
    assert_eq!(images.len(), MAX_IMAGES_PER_MESSAGE);
    assert_eq!(notes.len(), 1);
    assert!(notes[0].contains("image omitted"));
}

#[test]
fn request_projection_preserves_text_image_interleaving_and_tool_identity() {
    let dir = tempdir().unwrap();
    let image = asset(&dir);
    let mut tool = Message::tool("call-7", "caption before");
    tool.content.blocks.extend([
        ContentBlock::Image {
            image: ImageRef::Asset(image.clone()),
        },
        ContentBlock::Text {
            text: "caption after".into(),
        },
    ]);
    let prepared = prepare_request_images(
        ChatRequest::streaming("deepseek-flash", vec![tool]),
        Some(ImageDetail::Low),
        true,
    );
    let body = serde_json::to_value(&prepared.request).unwrap();
    assert_eq!(body["messages"][0]["role"], "tool");
    assert_eq!(body["messages"][0]["tool_call_id"], "call-7");
    assert_eq!(body["messages"][0]["content"][0]["text"], "caption before");
    assert_eq!(body["messages"][0]["content"][1]["type"], "image_url");
    assert_eq!(
        body["messages"][0]["content"][1]["image_url"]["detail"],
        "low"
    );
    assert_eq!(body["messages"][0]["content"][2]["text"], "caption after");
    assert_eq!(prepared.images.len(), 1);
    assert!(prepared.images[0].included);
    assert_eq!(prepared.images[0].call_id.as_deref(), Some("call-7"));
    assert!(prepared.images[0].image.contains(&image.digest));
}

#[test]
fn plain_text_requests_do_not_change_during_image_preparation() {
    let request = ChatRequest::streaming(
        "deepseek-flash",
        vec![Message::system("system"), Message::user("hello")],
    );
    let expected = serde_json::to_vec(&request).unwrap();
    let prepared = prepare_request_images(request, None, true);
    assert_eq!(serde_json::to_vec(&prepared.request).unwrap(), expected);
    assert!(prepared.images.is_empty());
}

#[test]
fn system_images_are_supported_and_assistant_images_are_reported_as_unsupported() {
    let dir = tempdir().unwrap();
    let image = asset(&dir);
    let system = with_image(Message::system("reference"), ImageRef::Asset(image.clone()));
    let assistant = with_image(
        Message::new(Role::Assistant, "answer"),
        ImageRef::Asset(image),
    );
    let prepared = prepare_request_images(
        ChatRequest::streaming("deepseek-flash", vec![system, assistant]),
        None,
        true,
    );
    let body = serde_json::to_value(&prepared.request).unwrap();
    assert_eq!(body["messages"][0]["content"][1]["type"], "image_url");
    assert!(prepared.images[0].included);
    assert!(!prepared.images[1].included);
    assert!(
        prepared.images[1]
            .reason
            .as_ref()
            .unwrap()
            .contains("assistant messages do not support images")
    );
    assert!(body["messages"][1]["content"].is_string());
}

#[test]
fn unavailable_observation_is_replaced_in_place_and_reported() {
    let dir = tempdir().unwrap();
    let image = asset(&dir);
    std::fs::remove_file(image.path()).unwrap();
    let mut message = with_image(
        Message::tool("call", "before"),
        ImageRef::Asset(image.clone()),
    );
    message.content.blocks.push(ContentBlock::Text {
        text: "after".into(),
    });
    let prepared = prepare_request_images(
        ChatRequest::streaming("deepseek-flash", vec![message]),
        None,
        true,
    );
    assert!(!prepared.request.messages[0].content.has_images());
    assert!(
        matches!(&prepared.request.messages[0].content.blocks[1], ContentBlock::Text { text } if text.contains("image unavailable"))
    );
    assert!(
        matches!(&prepared.request.messages[0].content.blocks[2], ContentBlock::Text { text } if text == "after")
    );
    assert!(!prepared.images[0].included);
    assert!(prepared.images[0].image.contains(&image.digest));
}

#[test]
fn fixed_nonvisual_route_reports_observation_not_included() {
    let dir = tempdir().unwrap();
    let message = with_image(Message::user("look"), ImageRef::Asset(asset(&dir)));
    let prepared = prepare_request_images(
        ChatRequest::streaming("deepseek-v4-pro", vec![message]),
        None,
        false,
    );
    assert!(!prepared.request.messages[0].content.has_images());
    assert!(
        prepared.images[0]
            .reason
            .as_ref()
            .unwrap()
            .contains("current model does not accept images")
    );
    assert!(!prepared.images[0].included);
}

#[test]
fn newest_images_receive_the_request_budget_first() {
    let dir = tempdir().unwrap();
    let image = asset(&dir);
    let messages = vec![
        with_image(Message::user("older"), ImageRef::Asset(image.clone())),
        with_image(Message::user("newer"), ImageRef::Asset(image.clone())),
    ];
    let prepared = prepare_request_images_with_limits(
        ChatRequest::streaming("deepseek-flash", messages),
        None,
        true,
        image.bytes,
        MAX_REQUEST_BYTES,
    );
    assert!(!prepared.request.messages[0].content.has_images());
    assert!(prepared.request.messages[1].content.has_images());
    assert_eq!(
        prepared
            .images
            .iter()
            .map(|image| image.included)
            .collect::<Vec<_>>(),
        [false, true]
    );
}

#[test]
fn message_limit_keeps_newest_images_without_reordering_blocks() {
    let dir = tempdir().unwrap();
    let image = asset(&dir);
    let mut message = Message::user("many");
    for _ in 0..MAX_IMAGES_PER_MESSAGE + 1 {
        message.content.push_image(ImageRef::Asset(image.clone()));
    }
    let prepared = prepare_request_images(
        ChatRequest::streaming("deepseek-flash", vec![message]),
        None,
        true,
    );
    assert_eq!(
        prepared.request.messages[0].content.image_count(),
        MAX_IMAGES_PER_MESSAGE
    );
    assert!(!prepared.images[0].included);
    assert!(prepared.images.iter().skip(1).all(|image| image.included));
}

#[test]
fn raw_inline_images_are_charged_against_the_same_budget() {
    let url = encode_data_url(ImageFormat::Png, PNG);
    let message = with_image(Message::user("look"), ImageRef::Url { url, detail: None });
    let prepared = prepare_request_images_with_limits(
        ChatRequest::streaming("deepseek-flash", vec![message]),
        None,
        true,
        1,
        MAX_REQUEST_BYTES,
    );
    assert!(!prepared.images[0].included);
    assert!(!prepared.request.messages[0].content.has_images());
}

#[test]
fn complete_request_budget_retains_original_asset_identity_in_omission() {
    let dir = tempdir().unwrap();
    let image = asset(&dir);
    let message = with_image(
        Message::tool("read", "look"),
        ImageRef::Asset(image.clone()),
    );
    let mut request = ChatRequest::streaming("deepseek-flash", vec![message]);
    request.messages[0].content.blocks.push(ContentBlock::Text {
        text: "x".repeat(2000),
    });
    let prepared = prepare_request_images_with_limits(request, None, true, MAX_TOTAL_BYTES, 2100);
    assert!(!prepared.images[0].included);
    assert!(
        prepared.images[0]
            .reason
            .as_ref()
            .unwrap()
            .contains("request body over")
    );
    assert!(
        prepared.images[0]
            .reason
            .as_ref()
            .unwrap()
            .contains(&image.digest)
    );
    assert!(!prepared.request.messages[0].content.has_images());
}

#[test]
fn low_changes_the_provider_parameter_without_claiming_local_resizing() {
    let dir = tempdir().unwrap();
    let image = asset(&dir);
    let message = with_image(Message::user("look"), ImageRef::Asset(image));
    let prepared = prepare_request_images(
        ChatRequest::streaming("deepseek-flash", vec![message]),
        Some(ImageDetail::Low),
        true,
    );
    let ImageRef::Url { url, detail } = prepared.request.messages[0]
        .content
        .images()
        .next()
        .unwrap()
    else {
        panic!("resolved image expected");
    };
    assert_eq!(*detail, Some(ImageDetail::Low));
    let encoded = url.split_once(";base64,").unwrap().1;
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .unwrap(),
        PNG
    );
}

#[test]
fn image_content_round_trip_preserves_asset_identity_and_order() {
    let dir = tempdir().unwrap();
    let content = Content::new(vec![
        ContentBlock::Text {
            text: "before".into(),
        },
        ContentBlock::Image { image: asset(&dir) },
        ContentBlock::Text {
            text: "after".into(),
        },
    ]);
    let decoded: Content = serde_json::from_str(&serde_json::to_string(&content).unwrap()).unwrap();
    assert_eq!(decoded, content);
    assert_eq!(decoded.images().next().unwrap().read().unwrap(), PNG);
}

#[test]
fn detail_only_accepts_supported_configuration_spellings() {
    for detail in [ImageDetail::Low, ImageDetail::Original, ImageDetail::Auto] {
        assert_eq!(ImageDetail::parse(detail.as_str()), Some(detail));
    }
    assert_eq!(ImageDetail::parse(" LOW "), Some(ImageDetail::Low));
    assert_eq!(ImageDetail::parse("medium"), None);
}

#[test]
fn inline_byte_estimation_is_bounded_for_empty_and_padding_only_values() {
    assert_eq!(inline_bytes("data:image/png;base64,"), Some(0));
    assert_eq!(inline_bytes("data:image/png;base64,="), Some(0));
    assert_eq!(inline_bytes("https://example.com/a.png"), None);
}
