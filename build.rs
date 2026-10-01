//! Gzip the text assets in `static/` into `OUT_DIR`, so the binary carries
//! them compressed and serves them with `Content-Encoding: gzip` as is.

use std::{env, fs, io::Write as _, path::Path};

use flate2::{Compression, write::GzEncoder};

const ASSETS: [&str; 4] = ["style.css", "app.js", "town.svg", "clouds.svg"];

fn main() {
    let out = env::var("OUT_DIR").unwrap();
    for name in ASSETS {
        let src = format!("static/{name}");
        println!("cargo::rerun-if-changed={src}");
        let plain = fs::read(&src).unwrap();
        // GzEncoder writes no name or mtime, so the output is reproducible.
        let mut gz = GzEncoder::new(Vec::new(), Compression::best());
        gz.write_all(&plain).unwrap();
        fs::write(
            Path::new(&out).join(format!("{name}.gz")),
            gz.finish().unwrap(),
        )
        .unwrap();
    }
}
