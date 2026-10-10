//! Embed the web page gzip-compressed (about 128 KB raw, 34 KB gzipped) so the
//! binary stays small and the server never compresses at runtime. Falls back to the
//! raw page when no `gzip` tool is available (e.g. a bare Windows build host).
use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=static/index.html");
    let out = Path::new(&std::env::var("OUT_DIR").unwrap()).join("index.html.bin");
    let gz = Command::new("gzip").args(["-9", "-n", "-c", "static/index.html"]).output();
    match gz {
        Ok(o) if o.status.success() && !o.stdout.is_empty() => {
            std::fs::write(&out, o.stdout).unwrap();
            println!("cargo:rustc-env=PAGE_ENCODING=gzip");
        }
        _ => {
            std::fs::copy("static/index.html", &out).unwrap();
            println!("cargo:rustc-env=PAGE_ENCODING=identity");
        }
    }
}
