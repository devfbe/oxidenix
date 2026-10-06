//! Decodes the boot logo (assets/logo.png) into raw RGB pixels blended onto
//! black, so the kernel needs no image decoder.

use std::{env, fs, path::Path};

fn main() {
    let src = "assets/logo.png";
    println!("cargo:rerun-if-changed={src}");
    let decoder = png::Decoder::new(std::io::BufReader::new(fs::File::open(src).expect("open logo")));
    let mut reader = decoder.read_info().expect("read logo header");
    let mut buf = vec![0; reader.output_buffer_size().expect("logo size")];
    let info = reader.next_frame(&mut buf).expect("decode logo");
    assert_eq!(
        (info.color_type, info.bit_depth),
        (png::ColorType::Rgba, png::BitDepth::Eight),
        "the logo must be 8-bit RGBA"
    );
    let rgb: Vec<u8> = buf[..info.buffer_size()]
        .chunks_exact(4)
        .flat_map(|p| {
            let a = p[3] as u16;
            [0, 1, 2].map(|i| (p[i] as u16 * a / 255) as u8)
        })
        .collect();
    let out = Path::new(&env::var("OUT_DIR").unwrap()).to_path_buf();
    fs::write(out.join("logo.rgb"), rgb).unwrap();
    fs::write(
        out.join("logo.rs"),
        format!("pub const LOGO_WIDTH: usize = {};\npub const LOGO_HEIGHT: usize = {};\n", info.width, info.height),
    )
    .unwrap();
}
