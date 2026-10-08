//! Path arithmetic. As the kernel's VFS did, ".." is resolved by name
//! (lexically, before symlinks are looked at): "/a/link/.." is "/a".

use alloc::string::{String, ToString};
use alloc::vec::Vec;

/// The components of `path` relative to `cwd` (absolute, without "." and
/// "..", and ".." of the root is the root).
pub fn normalize(cwd: &str, path: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |c: &str| match c {
        "" | "." => {}
        ".." => {
            out.pop();
        }
        c => out.push(c.to_string()),
    };
    if !path.starts_with('/') {
        cwd.split('/').for_each(&mut push);
    }
    path.split('/').for_each(&mut push);
    out
}

/// An absolute path from components ("/" for none).
pub fn join(components: &[String]) -> String {
    if components.is_empty() {
        return "/".to_string();
    }
    let mut out = String::new();
    for c in components {
        out.push('/');
        out.push_str(c);
    }
    out
}
