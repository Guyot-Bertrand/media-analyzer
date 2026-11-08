//! Tests sur le corpus généré (`cargo xtask corpus gen`) : le numéro incrusté est comparé à des
//! images décodées par `ffmpeg`. Les tests de la source d'AU sur TS sont dans `ma-ts`
//! (`crates/ma-ts/tests/source.rs`).

use std::process::Command;

use ma_decode::{PixelLayout, Planes, Rect, Size};
use ma_testkit::{corpus_file, read_frame_code};

fn run(cmd: &mut Command) -> Vec<u8> {
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "{cmd:?} : {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out.stdout
}

// --- Numéro incrusté, comparé à des images décodées par ffmpeg --------------------------------

/// Image `n` de `name` décodée par ffmpeg dans `pix_fmt`, à la taille `out` (mise à l'échelle).
fn ffmpeg_frame(name: &str, n: u32, pix_fmt: &str, out: Size) -> Vec<u8> {
    let vf = format!("select=eq(n\\,{n}),scale={}:{}", out.w, out.h);
    run(Command::new("ffmpeg")
        .args(["-v", "error", "-i"])
        .arg(corpus_file(name))
        .args([
            "-vf",
            &vf,
            "-frames:v",
            "1",
            "-fps_mode",
            "passthrough",
            "-f",
            "rawvideo",
            "-pix_fmt",
            pix_fmt,
            "-",
        ]))
}

fn luma_planes(layout: PixelLayout, raw: &[u8], size: Size, bytes: u32) -> Planes {
    let luma_len = (size.w * size.h * bytes) as usize;
    Planes {
        layout,
        size,
        planes: vec![raw[..luma_len].to_vec()],
        strides: vec![size.w * bytes],
    }
}

#[test]
fn frame_code_reads_real_decoded_frames() {
    let full = Size { w: 1280, h: 720 };
    let whole = Rect {
        x: 0,
        y: 0,
        w: 1280,
        h: 720,
    };
    for (name, pix_fmt, layout, bytes) in [
        ("h264_720p_closed", "gray", PixelLayout::Planar8, 1),
        ("h264_720p_closed", "nv12", PixelLayout::SemiPlanar8, 1),
        ("hevc_720p_main10", "yuv420p10le", PixelLayout::Planar10, 2),
        ("hevc_720p_main10", "p010le", PixelLayout::SemiPlanar10, 2),
    ] {
        for n in [0, 1, 137] {
            // Pleine taille, puis réduite de moitié et au quart (zoom « Ajusté »).
            for out in [full, Size { w: 640, h: 360 }, Size { w: 320, h: 180 }] {
                let raw = ffmpeg_frame(name, n, pix_fmt, out);
                let img = luma_planes(layout, &raw, out, bytes);
                assert_eq!(
                    read_frame_code(&img, whole, full),
                    Some(n),
                    "{name} {pix_fmt} image {n} en {}x{}",
                    out.w,
                    out.h
                );
            }
        }
    }
}

#[test]
fn frame_code_reads_a_one_to_one_region() {
    let full = Size { w: 1280, h: 720 };
    // Région 1:1 contenant la bande : bande et un peu de l'image.
    let region = Rect {
        x: 0,
        y: 0,
        w: 1280,
        h: 200,
    };
    let raw = ffmpeg_frame("h264_720p_closed", 42, "gray", full);
    let rows = (region.w * region.h) as usize;
    let img = Planes {
        layout: PixelLayout::Planar8,
        size: Size {
            w: region.w,
            h: region.h,
        },
        planes: vec![raw[..rows].to_vec()],
        strides: vec![region.w],
    };
    assert_eq!(read_frame_code(&img, region, full), Some(42));
    // Une région qui ne contient pas la bande ne donne rien.
    let below = Rect {
        x: 0,
        y: 300,
        w: 640,
        h: 200,
    };
    assert_eq!(read_frame_code(&img, below, full), None);
}
