"""Cache de pages d'un fichier, pour le banc NFS (tools/nfs-bench/run.sh, décision R24).

Usage (Python 3, bibliothèque standard seule ; à lancer avec « python3 -I ») :

    python3 -I tools/nfs-bench/cache.py drop FICHIER...
        retire du cache de pages les pages propres de chaque fichier (POSIX_FADV_DONTNEED) ;
        sans privilège, pour un fichier lisible.
    python3 -I tools/nfs-bench/cache.py resident FICHIER...
        affiche la part de pages résidentes de chaque fichier (mmap + mincore), sous la forme
        « FICHIER : R/N pages résidentes (P %) ».

Option --max-percent P (avec resident) : code de sortie 3 si un fichier dépasse P %.
Codes de sortie : 0 succès, 2 usage, 1 erreur d'E/S, 3 seuil dépassé.
"""

import ctypes
import ctypes.util
import mmap
import os
import sys

USAGE = "usage : cache.py drop FICHIER... | cache.py resident [--max-percent P] FICHIER..."


def drop(path):
    fd = os.open(path, os.O_RDONLY)
    try:
        os.posix_fadvise(fd, 0, 0, os.POSIX_FADV_DONTNEED)
    finally:
        os.close(fd)


def _libc():
    libc = ctypes.CDLL(ctypes.util.find_library("c") or "libc.so.6", use_errno=True)
    libc.mmap.restype = ctypes.c_void_p
    libc.mmap.argtypes = [
        ctypes.c_void_p,
        ctypes.c_size_t,
        ctypes.c_int,
        ctypes.c_int,
        ctypes.c_int,
        ctypes.c_long,
    ]
    libc.munmap.argtypes = [ctypes.c_void_p, ctypes.c_size_t]
    libc.mincore.argtypes = [ctypes.c_void_p, ctypes.c_size_t, ctypes.c_void_p]
    return libc


def resident(path, libc):
    """Rend (pages résidentes, pages du fichier). mmap ne lit rien : mincore ne fait que
    consulter le cache de pages, sans y ajouter de page."""
    fd = os.open(path, os.O_RDONLY)
    try:
        size = os.fstat(fd).st_size
        page = os.sysconf("SC_PAGE_SIZE")
        pages = (size + page - 1) // page
        if pages == 0:
            return 0, 0
        addr = libc.mmap(None, size, mmap.PROT_READ, mmap.MAP_SHARED, fd, 0)
        if addr in (None, ctypes.c_void_p(-1).value):
            raise OSError(ctypes.get_errno(), "mmap", path)
        try:
            vec = (ctypes.c_ubyte * pages)()
            if libc.mincore(ctypes.c_void_p(addr), size, vec) != 0:
                raise OSError(ctypes.get_errno(), "mincore", path)
            return sum(v & 1 for v in vec), pages
        finally:
            libc.munmap(ctypes.c_void_p(addr), size)
    finally:
        os.close(fd)


def main(argv):
    if len(argv) < 2 or argv[0] not in ("drop", "resident"):
        print(USAGE, file=sys.stderr)
        return 2
    cmd, args = argv[0], argv[1:]
    max_percent = None
    if cmd == "resident" and args[0] == "--max-percent":
        if len(args) < 3:
            print(USAGE, file=sys.stderr)
            return 2
        try:
            max_percent = float(args[1])
        except ValueError:
            print(f"seuil invalide : {args[1]}", file=sys.stderr)
            return 2
        args = args[2:]
    code = 0
    libc = _libc() if cmd == "resident" else None
    for path in args:
        try:
            if cmd == "drop":
                drop(path)
                continue
            res, pages = resident(path, libc)
        except OSError as e:
            print(f"{path} : {e.strerror or e}", file=sys.stderr)
            return 1
        percent = 100.0 * res / pages if pages else 0.0
        print(f"{path} : {res}/{pages} pages résidentes ({percent:.1f} %)")
        if max_percent is not None and percent > max_percent:
            code = 3
    return code


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
