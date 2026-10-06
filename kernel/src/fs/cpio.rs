//! Entpackt ein cpio-Archiv im "newc"-Format in das VFS.

use super::{mkdir_p, Data, Inode, Node, S_IFDIR, S_IFLNK, S_IFMT, S_IFREG};
use alloc::string::ToString;
use alloc::sync::Arc;

const HEADER_LEN: usize = 110;

fn hex(field: &[u8]) -> Result<usize, &'static str> {
    let s = core::str::from_utf8(field).map_err(|_| "Header kein ASCII")?;
    usize::from_str_radix(s, 16).map_err(|_| "Header-Feld kein Hex")
}

fn align4(x: usize) -> usize {
    (x + 3) & !3
}

pub fn unpack(root: &Arc<Inode>, data: &'static [u8]) -> Result<(), &'static str> {
    let mut pos = 0;
    loop {
        let header = data.get(pos..pos + HEADER_LEN).ok_or("Archiv abgeschnitten")?;
        if &header[0..6] != b"070701" {
            return Err("falsche cpio-Magic");
        }
        let field = |i: usize| hex(&header[6 + i * 8..6 + (i + 1) * 8]);
        let mode = field(1)? as u32;
        let filesize = field(6)?;
        let namesize = field(11)?;

        let name_start = pos + HEADER_LEN;
        let name = data
            .get(name_start..name_start + namesize.saturating_sub(1))
            .ok_or("Name abgeschnitten")?;
        let name = core::str::from_utf8(name).map_err(|_| "Name kein UTF-8")?;
        let data_start = align4(name_start + namesize);
        let body = data.get(data_start..data_start + filesize).ok_or("Daten abgeschnitten")?;
        pos = align4(data_start + filesize);

        if name == "TRAILER!!!" {
            return Ok(());
        }
        let (dir, base) = match name.rsplit_once('/') {
            Some((d, b)) => (mkdir_p(root, d), b),
            None => (root.clone(), name),
        };
        if base.is_empty() || base == "." {
            continue;
        }
        let node = match mode & S_IFMT {
            S_IFDIR => {
                mkdir_p(&dir, base);
                continue;
            }
            S_IFREG => Node::File(Data::Static(body)),
            S_IFLNK => Node::Symlink(core::str::from_utf8(body).map_err(|_| "Symlink kein UTF-8")?.to_string()),
            _ => continue,
        };
        let _ = dir.insert(base, Inode::new(node, mode));
    }
}
