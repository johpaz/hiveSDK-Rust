#![cfg(feature = "images")]
use hive_agent_core::{
    Error,
    artifacts::NewArtifact,
    image_transform::{Crop, Format, Transform, apply},
    sessions::SessionStore,
};
use image_codec::{
    ExtendedColorType, ImageEncoder, Rgba, RgbaImage,
    codecs::{gif::GifEncoder, jpeg::JpegEncoder, png::PngEncoder},
};
use serde_json::json;

const RED: [u8; 4] = [255, 0, 0, 255];
const GREEN: [u8; 4] = [0, 255, 0, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];
const WHITE: [u8; 4] = [255, 255, 255, 255];

/// Four quadrants: red top-left, green top-right, blue bottom-left, white bottom-right.
fn quadrants(width: u32, height: u32) -> RgbaImage {
    RgbaImage::from_fn(width, height, |x, y| {
        Rgba(match (x < width / 2, y < height / 2) {
            (true, true) => RED,
            (false, true) => GREEN,
            (true, false) => BLUE,
            (false, false) => WHITE,
        })
    })
}
fn png_of(image: &RgbaImage) -> Vec<u8> {
    let mut out = vec![];
    PngEncoder::new(&mut out)
        .write_image(
            image.as_raw(),
            image.width(),
            image.height(),
            ExtendedColorType::Rgba8,
        )
        .unwrap();
    out
}
fn decode(bytes: &[u8]) -> RgbaImage {
    image_codec::load_from_memory(bytes).unwrap().to_rgba8()
}
fn close(a: [u8; 4], b: [u8; 4], tolerance: i32) -> bool {
    a.iter()
        .zip(b)
        .all(|(a, b)| (i32::from(*a) - i32::from(b)).abs() <= tolerance)
}
fn pixel(image: &RgbaImage, x: u32, y: u32) -> [u8; 4] {
    image.get_pixel(x, y).0
}
fn run(
    bytes: &[u8],
    transform: Transform,
) -> hive_agent_core::Result<hive_agent_core::image_transform::Transformed> {
    apply(bytes, &transform)
}
fn message(error: Error) -> String {
    match error {
        Error::Input(message) | Error::State(message) => message,
        other => other.to_string(),
    }
}

#[test]
fn crop_rotate_and_mirrors_move_the_right_pixels() {
    let source = png_of(&quadrants(4, 4));
    let cropped = run(
        &source,
        Transform {
            crop: Some(Crop {
                x: 0,
                y: 0,
                width: 2,
                height: 2,
            }),
            ..Default::default()
        },
    )
    .unwrap();
    let image = decode(&cropped.bytes);
    assert_eq!(
        (cropped.width, cropped.height, cropped.format),
        (2, 2, Format::Png)
    );
    assert!(
        image.pixels().all(|p| p.0 == RED),
        "only the red quadrant is left"
    );

    // 90 degrees clockwise: the top-left (red) goes to the top-right.
    let rotated = decode(
        &run(
            &source,
            Transform {
                rotate: Some(90),
                ..Default::default()
            },
        )
        .unwrap()
        .bytes,
    );
    assert_eq!(
        (
            pixel(&rotated, 3, 0),
            pixel(&rotated, 0, 0),
            pixel(&rotated, 0, 3),
            pixel(&rotated, 3, 3)
        ),
        (RED, BLUE, WHITE, GREEN)
    );
    let upside = decode(
        &run(
            &source,
            Transform {
                rotate: Some(180),
                ..Default::default()
            },
        )
        .unwrap()
        .bytes,
    );
    assert_eq!((pixel(&upside, 3, 3), pixel(&upside, 0, 0)), (RED, WHITE));
    let flipped = decode(
        &run(
            &source,
            Transform {
                flip: true,
                ..Default::default()
            },
        )
        .unwrap()
        .bytes,
    );
    assert_eq!(
        (pixel(&flipped, 0, 0), pixel(&flipped, 0, 3)),
        (BLUE, RED),
        "flip mirrors top to bottom"
    );
    let flopped = decode(
        &run(
            &source,
            Transform {
                flop: true,
                ..Default::default()
            },
        )
        .unwrap()
        .bytes,
    );
    assert_eq!(
        (pixel(&flopped, 0, 0), pixel(&flopped, 3, 0)),
        (GREEN, RED),
        "flop mirrors left to right"
    );
    // Crop first, then rotate: a 4x2 strip rotated is 2x4.
    let both = run(
        &source,
        Transform {
            crop: Some(Crop {
                x: 0,
                y: 0,
                width: 4,
                height: 2,
            }),
            rotate: Some(90),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!((both.width, both.height), (2, 4));
}

#[test]
fn fitting_keeps_the_proportions_and_never_enlarges() {
    let source = png_of(&quadrants(400, 200));
    let fit = |max_width, max_height| {
        let done = run(
            &source,
            Transform {
                max_width,
                max_height,
                ..Default::default()
            },
        )
        .unwrap();
        (done.width, done.height)
    };
    assert_eq!(fit(Some(100), None), (100, 50));
    assert_eq!(fit(None, Some(50)), (100, 50));
    assert_eq!(fit(Some(100), Some(100)), (100, 50), "inside the box");
    assert_eq!(fit(Some(1000), Some(1000)), (400, 200), "never enlarged");
    let small = decode(
        &run(
            &source,
            Transform {
                max_width: Some(40),
                ..Default::default()
            },
        )
        .unwrap()
        .bytes,
    );
    assert!(
        close(pixel(&small, 5, 5), RED, 8) && close(pixel(&small, 35, 15), WHITE, 8),
        "content survives the resize"
    );
}

#[test]
fn formats_convert_and_transparency_is_flattened_onto_white_for_jpeg() {
    let source = png_of(&quadrants(8, 8));
    let jpeg = run(
        &source,
        Transform {
            format: Some(Format::Jpeg),
            quality: Some(95),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        (&jpeg.bytes[..3], jpeg.format.mime_type()),
        (&[0xff, 0xd8, 0xff][..], "image/jpeg")
    );
    assert!(close(pixel(&decode(&jpeg.bytes), 1, 1), RED, 12));
    let low = run(
        &source,
        Transform {
            format: Some(Format::Jpeg),
            quality: Some(5),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(
        low.bytes.len() < jpeg.bytes.len(),
        "quality changes the size"
    );

    let webp = run(
        &source,
        Transform {
            format: Some(Format::Webp),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(&webp.bytes[8..12], b"WEBP");
    assert_eq!(
        decode(&webp.bytes),
        quadrants(8, 8),
        "WebP is written lossless: identical pixels"
    );
    let back = run(
        &webp.bytes,
        Transform {
            format: Some(Format::Png),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        decode(&back.bytes),
        quadrants(8, 8),
        "and can be read again"
    );

    let clear = RgbaImage::from_pixel(4, 4, Rgba([0, 0, 0, 0]));
    let flat = run(
        &png_of(&clear),
        Transform {
            format: Some(Format::Jpeg),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(
        close(pixel(&decode(&flat.bytes), 2, 2), WHITE, 6),
        "transparent becomes white, not black"
    );
    let half = RgbaImage::from_pixel(4, 4, Rgba([0, 0, 0, 128]));
    let mid = pixel(
        &decode(
            &run(
                &png_of(&half),
                Transform {
                    format: Some(Format::Jpeg),
                    quality: Some(100),
                    ..Default::default()
                },
            )
            .unwrap()
            .bytes,
        ),
        2,
        2,
    );
    assert!(
        (100..160).contains(&i32::from(mid[0])),
        "half transparent black is mid grey: {mid:?}"
    );

    let gray = decode(
        &run(
            &source,
            Transform {
                grayscale: true,
                ..Default::default()
            },
        )
        .unwrap()
        .bytes,
    );
    assert!(
        gray.pixels().all(|p| p[0] == p[1] && p[1] == p[2]),
        "grayscale has equal channels"
    );
}

#[test]
fn a_gif_is_read_as_its_first_frame_and_leaves_as_png() {
    let mut bytes = vec![];
    {
        let mut encoder = GifEncoder::new(&mut bytes);
        encoder
            .encode(quadrants(6, 6).as_raw(), 6, 6, ExtendedColorType::Rgba8)
            .unwrap();
    }
    let done = run(
        &bytes,
        Transform {
            rotate: Some(180),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!((done.format, &done.bytes[1..4]), (Format::Png, &b"PNG"[..]));
    assert_eq!((done.width, done.height), (6, 6));
}

/// Insert an EXIF APP1 segment with the given orientation right after the JPEG's SOI.
fn with_orientation(jpeg: &[u8], orientation: u16) -> Vec<u8> {
    let mut exif = b"Exif\0\0II*\0".to_vec();
    exif.extend_from_slice(&8u32.to_le_bytes());
    exif.extend_from_slice(&1u16.to_le_bytes());
    exif.extend_from_slice(&0x0112u16.to_le_bytes());
    exif.extend_from_slice(&3u16.to_le_bytes());
    exif.extend_from_slice(&1u32.to_le_bytes());
    exif.extend_from_slice(&orientation.to_le_bytes());
    exif.extend_from_slice(&[0, 0]);
    exif.extend_from_slice(&0u32.to_le_bytes());
    let mut out = jpeg[..2].to_vec();
    out.extend_from_slice(&[0xff, 0xe1]);
    out.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
    out.extend_from_slice(&exif);
    out.extend_from_slice(&jpeg[2..]);
    out
}

#[test]
fn the_exif_orientation_of_a_phone_photo_is_applied() {
    let rgb = image_codec::DynamicImage::ImageRgba8(quadrants(8, 4)).to_rgb8();
    let mut plain = vec![];
    JpegEncoder::new_with_quality(&mut plain, 100)
        .write_image(rgb.as_raw(), 8, 4, ExtendedColorType::Rgb8)
        .unwrap();
    let straight = run(
        &plain,
        Transform {
            format: Some(Format::Png),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!((straight.width, straight.height), (8, 4));
    // Orientation 6 means "rotate 90 clockwise to display": the stored 8x4 shows as 4x8.
    let tagged = run(
        &with_orientation(&plain, 6),
        Transform {
            format: Some(Format::Png),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        (tagged.width, tagged.height),
        (4, 8),
        "upright, not sideways"
    );
    assert!(
        close(pixel(&decode(&tagged.bytes), 3, 1), RED, 20),
        "the red corner went to the top-right"
    );
}

#[test]
fn hostile_and_invalid_inputs_fail_cleanly_before_any_large_allocation() {
    // A header that claims 100 megapixels is refused before any decoding.
    let mut huge = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    huge.extend_from_slice(&10_000u32.to_be_bytes());
    huge.extend_from_slice(&10_000u32.to_be_bytes());
    huge.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
    let started = std::time::Instant::now();
    let error = message(
        run(
            &huge,
            Transform {
                rotate: Some(90),
                ..Default::default()
            },
        )
        .err()
        .unwrap(),
    );
    assert!(error.contains("megapixels"), "{error}");
    assert!(started.elapsed().as_secs() < 2);
    // A real header with a truncated body is a clean error, not a panic.
    let good = png_of(&quadrants(64, 64));
    let error = message(
        run(
            &good[..good.len() / 2],
            Transform {
                rotate: Some(90),
                ..Default::default()
            },
        )
        .err()
        .unwrap(),
    );
    assert!(error.contains("decoded"), "{error}");
    assert!(
        message(
            run(
                b"definitely not an image",
                Transform {
                    rotate: Some(90),
                    ..Default::default()
                }
            )
            .err()
            .unwrap()
        )
        .contains("recognised")
    );
    assert!(
        run(
            &[],
            Transform {
                rotate: Some(90),
                ..Default::default()
            }
        )
        .is_err()
    );

    let source = png_of(&quadrants(4, 4));
    for (transform, why) in [
        (Transform::default(), "nothing to do"),
        (
            Transform {
                rotate: Some(45),
                ..Default::default()
            },
            "rotate 45",
        ),
        (
            Transform {
                crop: Some(Crop {
                    x: 3,
                    y: 0,
                    width: 2,
                    height: 2,
                }),
                ..Default::default()
            },
            "crop outside",
        ),
        (
            Transform {
                crop: Some(Crop {
                    x: u32::MAX,
                    y: 0,
                    width: 2,
                    height: 2,
                }),
                ..Default::default()
            },
            "crop overflow",
        ),
        (
            Transform {
                crop: Some(Crop {
                    x: 0,
                    y: 0,
                    width: 0,
                    height: 2,
                }),
                ..Default::default()
            },
            "empty crop",
        ),
        (
            Transform {
                max_width: Some(0),
                ..Default::default()
            },
            "zero box",
        ),
        (
            Transform {
                max_height: Some(20_000),
                ..Default::default()
            },
            "box too big",
        ),
        (
            Transform {
                quality: Some(0),
                format: Some(Format::Jpeg),
                ..Default::default()
            },
            "quality 0",
        ),
        (
            Transform {
                quality: Some(101),
                ..Default::default()
            },
            "quality 101",
        ),
    ] {
        assert!(
            matches!(run(&source, transform), Err(Error::Input(_))),
            "{why}"
        );
    }
}

#[test]
fn the_tool_makes_a_new_artifact_and_keeps_the_source() {
    let dir = tempfile::tempdir().unwrap();
    let store = SessionStore::open(dir.path().join("db"), "tenant".into()).unwrap();
    let artifacts = store.artifacts(dir.path().join("files")).unwrap();
    let source = artifacts
        .create(
            &png_of(&quadrants(40, 20)),
            NewArtifact {
                kind: "photo".into(),
                mime_type: "image/png".into(),
                run_id: Some("run-1".into()),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        artifacts
            .tool_definitions()
            .iter()
            .any(|tool| tool.name == "artifact_transform_image")
    );
    let result = artifacts
        .execute_tool(
            "artifact_transform_image",
            json!({"id":source.id,"max_width":10,"format":"jpeg","quality":80,"crop":{"x":0,"y":0,"width":40,"height":20}}),
        )
        .unwrap();
    assert_eq!(
        (
            result["width"].clone(),
            result["height"].clone(),
            result["source"].clone()
        ),
        (json!(10), json!(5), json!(source.id))
    );
    let made = &result["artifact"];
    assert_eq!(
        (
            made["mime_type"].clone(),
            made["kind"].clone(),
            made["run_id"].clone()
        ),
        (json!("image/jpeg"), json!("image"), json!("run-1"))
    );
    assert_ne!(made["id"], json!(source.id));
    let bytes = artifacts.read_bytes(made["id"].as_str().unwrap()).unwrap();
    assert_eq!((decode(&bytes).width(), decode(&bytes).height()), (10, 5));
    assert!(
        artifacts.read_bytes(&source.id).is_ok(),
        "the source is untouched"
    );

    for (arguments, why) in [
        (json!({"id":source.id}), "no change requested"),
        (json!({"id":source.id,"rotate":45}), "schema"),
        (
            json!({"id":source.id,"rotate":90,"extra":1}),
            "unknown field",
        ),
        (
            json!({"id":"00000000-0000-4000-8000-000000000000","rotate":90}),
            "unknown artifact",
        ),
    ] {
        assert!(
            artifacts
                .execute_tool("artifact_transform_image", arguments)
                .is_err(),
            "{why}"
        );
    }
    let text = artifacts
        .create(
            b"hola",
            NewArtifact {
                kind: "note".into(),
                mime_type: "text/plain".into(),
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        artifacts
            .execute_tool(
                "artifact_transform_image",
                json!({"id":text.id,"rotate":90})
            )
            .is_err(),
        "not an image"
    );
    let other = store
        .for_tenant("other".into())
        .unwrap()
        .artifacts(dir.path().join("files"))
        .unwrap();
    assert!(
        other
            .execute_tool(
                "artifact_transform_image",
                json!({"id":source.id,"rotate":90})
            )
            .is_err(),
        "another tenant cannot transform it"
    );
}
