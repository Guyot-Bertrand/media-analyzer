//! Localité du système de fichiers (local ou réseau), d'après `/proc/self/mountinfo`.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Locality {
    Local,
    Remote,
}

/// Types de systèmes de fichiers considérés comme distants.
#[must_use]
pub fn fs_locality(fs_type: &str) -> Locality {
    match fs_type {
        "nfs" | "nfs4" | "cifs" | "smb3" | "fuse.sshfs" => Locality::Remote,
        _ => Locality::Local,
    }
}

/// Localité d'un chemin : point de montage le plus long qui préfixe le chemin canonique.
/// Toute impossibilité de lecture donne `Local`.
#[must_use]
pub fn path_locality(path: &Path) -> Locality {
    let Ok(canon) = std::fs::canonicalize(path) else {
        return Locality::Local;
    };
    let Ok(info) = std::fs::read("/proc/self/mountinfo") else {
        return Locality::Local;
    };
    locality_from_mountinfo(&String::from_utf8_lossy(&info), &canon)
}

pub(crate) fn locality_from_mountinfo(content: &str, canonical: &Path) -> Locality {
    let mut best: Option<(usize, Locality)> = None;
    for line in content.lines() {
        let fields: Vec<&str> = line.split(' ').collect();
        let Some(dash) = fields.iter().position(|f| *f == "-") else {
            continue;
        };
        let (Some(mount), Some(fs_type)) = (fields.get(4), fields.get(dash + 1)) else {
            continue;
        };
        let mount = unescape_octal(mount);
        let mount_path = Path::new(OsStr::from_bytes(&mount));
        if canonical.starts_with(mount_path) {
            let depth = mount_path.components().count();
            if best.is_none_or(|(d, _)| depth >= d) {
                best = Some((depth, fs_locality(fs_type)));
            }
        }
    }
    best.map_or(Locality::Local, |(_, l)| l)
}

/// Décode les échappements `\ooo` (octal) de mountinfo.
pub(crate) fn unescape_octal(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c))
        {
            let v = u32::from(b[i + 1] - b'0') * 64
                + u32::from(b[i + 2] - b'0') * 8
                + u32::from(b[i + 3] - b'0');
            out.push(u8::try_from(v & 0xff).unwrap_or(b'?'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn fs_type_mapping() {
        for t in ["nfs", "nfs4", "cifs", "smb3", "fuse.sshfs"] {
            assert_eq!(fs_locality(t), Locality::Remote, "{t}");
        }
        for t in [
            "ext4",
            "xfs",
            "btrfs",
            "tmpfs",
            "overlay",
            "fuse.mergerfs",
            "",
        ] {
            assert_eq!(fs_locality(t), Locality::Local, "{t}");
        }
    }

    #[test]
    fn tmp_is_local() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(path_locality(dir.path()), Locality::Local);
        assert_eq!(
            path_locality(Path::new("/nonexistent/zzz")),
            Locality::Local
        );
    }

    const MOUNTINFO: &str = "\
22 28 0:21 / /proc rw,nosuid - proc proc rw
30 22 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw
40 30 0:40 / /mnt/nas rw,relatime shared:5 master:2 - nfs4 srv:/export rw,vers=4.2
41 40 0:41 / /mnt/nas/local rw - ext4 /dev/sdb1 rw
42 30 0:42 / /mnt/my\\040share rw - cifs //srv/s rw
43 30 0:43 / /mnt/nasty rw - ext4 /dev/sdc1 rw
garbage line
";

    #[test]
    fn mountinfo_longest_prefix_wins() {
        let l = |p: &str| locality_from_mountinfo(MOUNTINFO, Path::new(p));
        assert_eq!(l("/home/x/video.ts"), Locality::Local);
        assert_eq!(l("/mnt/nas/video.ts"), Locality::Remote);
        assert_eq!(l("/mnt/nas"), Locality::Remote);
        assert_eq!(l("/mnt/nas/local/video.ts"), Locality::Local);
        assert_eq!(l("/mnt/nasty/video.ts"), Locality::Local);
        assert_eq!(l("/mnt/my share/a/b.ts"), Locality::Remote);
    }

    #[test]
    fn mountinfo_unreadable_or_empty_is_local() {
        assert_eq!(
            locality_from_mountinfo("", Path::new("/a")),
            Locality::Local
        );
        assert_eq!(
            locality_from_mountinfo("1 2 3 - nfs", Path::new("/a")),
            Locality::Local
        );
    }

    #[test]
    fn octal_unescape() {
        assert_eq!(unescape_octal("a\\040b\\134c"), b"a b\\c");
        assert_eq!(unescape_octal("a\\04"), b"a\\04");
        assert_eq!(unescape_octal("plain"), b"plain");
    }
}
