use std::process::Command;

fn main() {
    let out = std::env::var("OUT_DIR").unwrap();
    println!("cargo:rerun-if-changed=../userspace");
    let status = Command::new("../userspace/build.sh")
        .arg(&out)
        .status()
        .expect("userspace/build.sh nicht ausfuehrbar");
    assert!(status.success(), "Userspace-Build fehlgeschlagen");
}
