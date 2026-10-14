//! Path arithmetic. As the kernel's VFS did, ".." is resolved by name
//! (lexically, before symlinks are looked at): "/a/link/.." is "/a".
//!
//! The `try_` forms allocate only fallibly (`try_reserve`): the Linux
//! server answers ENOMEM when they fail (paths are what programs choose).

use alloc::collections::TryReserveError;
use alloc::string::String;
use alloc::vec::Vec;

/// The components of `path` relative to `cwd` (absolute, without "." and
/// "..", and ".." of the root is the root).
pub fn normalize(cwd: &str, path: &str) -> Vec<String> {
    try_normalize(cwd, path).expect("memory for a path")
}

/// `normalize`, failing when memory cannot be had.
pub fn try_normalize(cwd: &str, path: &str) -> Result<Vec<String>, TryReserveError> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |c: &str| -> Result<(), TryReserveError> {
        match c {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            c => {
                let mut s = String::new();
                s.try_reserve_exact(c.len())?;
                s.push_str(c);
                out.try_reserve(1)?;
                out.push(s);
            }
        }
        Ok(())
    };
    if !path.starts_with('/') {
        cwd.split('/').try_for_each(&mut push)?;
    }
    path.split('/').try_for_each(&mut push)?;
    Ok(out)
}

/// An absolute path from components ("/" for none).
pub fn join(components: &[String]) -> String {
    try_join(components).expect("memory for a path")
}

/// `join`, failing when memory cannot be had.
pub fn try_join(components: &[String]) -> Result<String, TryReserveError> {
    let len = components.iter().map(|c| c.len() + 1).sum::<usize>().max(1);
    let mut out = String::new();
    out.try_reserve_exact(len)?;
    if components.is_empty() {
        out.push('/');
    }
    for c in components {
        out.push('/');
        out.push_str(c);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn normalize_and_join() {
        assert_eq!(normalize("/a/b", "../c/./d//"), vec!["a", "c", "d"]);
        assert_eq!(normalize("/a", "/../x"), vec!["x"]);
        assert_eq!(join(&normalize("/", "")), "/");
        assert_eq!(join(&normalize("/a", "b/..")), "/a");
        assert_eq!(try_join(&[String::from("x"), String::from("yz")]).unwrap(), "/x/yz");
    }
}
