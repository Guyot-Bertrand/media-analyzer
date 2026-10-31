//! Source d'AU sur Transport Stream, pour les tests et les bancs uniquement.
//!
//! Hypothèse « 1 PES = 1 AU » (vraie pour le corpus généré par `FFmpeg`). À l'ouverture, un seul
//! passage sur le fichier lit la PAT et la PMT, garde la position et le PTS de chaque PES vidéo du
//! premier flux vidéo, et classe chaque image d'après ses premiers octets. Les AU sont ensuite
//! relues à la demande par `pread`. Les PTS ne sont pas dépliés (pas de bouclage sur 33 bits).

use std::fs::File;
use std::io::{self, BufReader, Read};
use std::os::unix::fs::FileExt;
use std::path::Path;

use ma_decode::{AuSource, Codec, FrameEntry, Leading, PictureType, RapKind};

use crate::nal::{NalLeading, classify};

/// Taille maximale d'une AU relue (32 Mio) ; au-delà, `read_au` échoue.
pub const MAX_AU_BYTES: usize = 32 * 1024 * 1024;

const TS_PACKET: usize = 188;
/// Octets de début d'AU conservés à l'ouverture pour la classification.
const HEAD_MAX: usize = 16 * 1024;
const READ_CHUNK: usize = TS_PACKET * 1024;
/// Marge au-delà de `MAX_AU_BYTES` pour l'en-tête PES, retiré avant le contrôle final.
const PES_SLACK: usize = 64;

thread_local! {
    static SCRATCH: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

struct Packet<'a> {
    pusi: bool,
    pid: u16,
    payload: &'a [u8],
}

fn parse_packet(p: &[u8]) -> io::Result<Packet<'_>> {
    if p[0] != 0x47 {
        return Err(invalid("paquet TS désynchronisé (octet 0x47 attendu)"));
    }
    let afc = (p[3] >> 4) & 3;
    let mut start = 4;
    if afc & 2 != 0 {
        start += 1 + usize::from(p[4]);
    }
    let payload = if afc & 1 != 0 && start < TS_PACKET {
        &p[start..]
    } else {
        &[]
    };
    Ok(Packet {
        pusi: p[1] & 0x40 != 0,
        pid: (u16::from(p[1] & 0x1F) << 8) | u16::from(p[2]),
        payload,
    })
}

/// Section PSI contenue dans un paquet à début d'unité de charge utile.
fn psi_section(payload: &[u8], table_id: u8) -> io::Result<&[u8]> {
    let short = || invalid("section PSI tronquée ou répartie sur plusieurs paquets");
    let pointer = usize::from(*payload.first().ok_or_else(short)?);
    let sec = payload.get(1 + pointer..).ok_or_else(short)?;
    if sec.first() != Some(&table_id) {
        return Err(invalid(format!("table PSI {table_id:#x} attendue")));
    }
    let len = 3
        + ((usize::from(*sec.get(1).ok_or_else(short)?) & 0x0F) << 8)
        + usize::from(*sec.get(2).ok_or_else(short)?);
    // Les 4 derniers octets sont le CRC ; ils ne portent pas d'entrée.
    if len < 12 {
        return Err(short());
    }
    sec.get(..len - 4).ok_or_else(short)
}

/// PID de la PMT du premier programme.
fn parse_pat(payload: &[u8]) -> io::Result<u16> {
    let sec = psi_section(payload, 0)?;
    sec[8..]
        .as_chunks::<4>()
        .0
        .iter()
        .find(|e| e[0] != 0 || e[1] != 0)
        .map(|e| (u16::from(e[2] & 0x1F) << 8) | u16::from(e[3]))
        .ok_or_else(|| invalid("PAT sans programme"))
}

fn codec_of(stream_type: u8) -> Option<Codec> {
    match stream_type {
        0x1B => Some(Codec::H264),
        0x24 => Some(Codec::Hevc),
        0x02 => Some(Codec::Mpeg2),
        _ => None,
    }
}

/// PID et codec du premier flux vidéo reconnu de la PMT.
fn parse_pmt(payload: &[u8]) -> io::Result<(u16, Codec)> {
    let short = || invalid("PMT tronquée");
    let sec = psi_section(payload, 2)?;
    if sec.len() < 12 {
        return Err(short());
    }
    let program_info = (usize::from(sec[10] & 0x0F) << 8) | usize::from(sec[11]);
    let mut rest = sec.get(12 + program_info..).ok_or_else(short)?;
    while rest.len() >= 5 {
        let info = (usize::from(rest[3] & 0x0F) << 8) | usize::from(rest[4]);
        if let Some(codec) = codec_of(rest[0]) {
            return Ok(((u16::from(rest[1] & 0x1F) << 8) | u16::from(rest[2]), codec));
        }
        rest = rest.get(5 + info..).ok_or_else(short)?;
    }
    Err(invalid(
        "aucun flux vidéo (H.264, HEVC, MPEG-2) dans la PMT",
    ))
}

/// Début du flux élémentaire et PTS d'un PES.
fn pes_header(buf: &[u8]) -> io::Result<(usize, Option<i64>)> {
    if buf.len() < 9 || buf[..3] != [0, 0, 1] {
        return Err(invalid("en-tête PES absent"));
    }
    let header_len = usize::from(buf[8]);
    let es = 9 + header_len;
    if buf.len() < es {
        return Err(invalid("en-tête PES tronqué"));
    }
    let pts = if buf[7] & 0x80 != 0 {
        if header_len < 5 {
            return Err(invalid("en-tête PES : champ PTS tronqué"));
        }
        let b = &buf[9..14];
        Some(
            (i64::from(b[0] >> 1 & 7) << 30)
                | (i64::from(b[1]) << 22)
                | (i64::from(b[2] >> 1) << 15)
                | (i64::from(b[3]) << 7)
                | i64::from(b[4] >> 1),
        )
    } else {
        None
    };
    Ok((es, pts))
}

#[derive(Debug, Clone, Copy)]
struct Loc {
    offset: u64,
    end: u64,
}

struct Raw {
    loc: Loc,
    pts: i64,
    picture: PictureType,
    rap: RapKind,
    nal_leading: Option<NalLeading>,
}

struct Pending {
    offset: u64,
    head: Vec<u8>,
}

fn finish(p: &Pending, end: u64, codec: Codec, index: usize) -> io::Result<Raw> {
    let (es, pts) = pes_header(&p.head)?;
    let pts = pts.ok_or_else(|| invalid(format!("PES {index} sans PTS")))?;
    let (picture, rap, nal_leading) = classify(codec, &p.head[es..]);
    Ok(Raw {
        loc: Loc {
            offset: p.offset,
            end,
        },
        pts,
        picture,
        rap,
        nal_leading,
    })
}

/// Un passage sur le fichier : flux vidéo choisi et une `Raw` par PES, en ordre de décodage.
fn scan(file: &File) -> io::Result<(u16, Codec, Vec<Raw>)> {
    let mut reader = BufReader::with_capacity(1 << 20, file);
    let mut pkt = [0u8; TS_PACKET];
    let mut pos = 0u64;
    let mut pmt_pid = None;
    let mut video: Option<(u16, Codec)> = None;
    let mut cur: Option<Pending> = None;
    let mut raws: Vec<Raw> = Vec::new();
    loop {
        match reader.read_exact(&mut pkt) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e),
        }
        let at = pos;
        pos += TS_PACKET as u64;
        let p = parse_packet(&pkt)?;
        if p.pid == 0 && p.pusi && pmt_pid.is_none() {
            pmt_pid = Some(parse_pat(p.payload)?);
        } else if Some(p.pid) == pmt_pid && p.pusi && video.is_none() {
            video = Some(parse_pmt(p.payload)?);
        } else if let Some((_, codec)) = video.filter(|v| v.0 == p.pid) {
            if p.pusi {
                if let Some(prev) = cur.take() {
                    raws.push(finish(&prev, at, codec, raws.len())?);
                }
                cur = Some(Pending {
                    offset: at,
                    head: Vec::new(),
                });
            }
            if let Some(c) = cur.as_mut().filter(|c| c.head.len() < HEAD_MAX) {
                c.head.extend_from_slice(p.payload);
            }
        }
    }
    let (pid, codec) =
        video.ok_or_else(|| invalid("aucune PMT avec flux vidéo dans le fichier"))?;
    if let Some(last) = cur.take() {
        raws.push(finish(&last, pos, codec, raws.len())?);
    }
    Ok((pid, codec, raws))
}

/// Statut `Leading` de chaque image, d'après le type NAL (HEVC) ou l'ordre de présentation
/// (H.264, MPEG-2) : une image qui précède son RAP en présentation tout en le suivant en décodage
/// est `Skippable`, sauf si le RAP est une IDR ou un GOP MPEG-2 fermé (`Decodable`).
fn leading_of(codec: Codec, raw: &Raw, pres: u32, rap: Option<(u32, RapKind)>) -> Leading {
    if raw.rap != RapKind::None {
        return Leading::None;
    }
    if codec == Codec::Hevc {
        return match raw.nal_leading {
            Some(NalLeading::Skippable) => Leading::Skippable,
            Some(NalLeading::Decodable) => Leading::Decodable,
            None => Leading::None,
        };
    }
    match rap {
        Some((rap_pres, kind)) if pres < rap_pres => {
            if matches!(kind, RapKind::Idr | RapKind::Intra { closed_gop: true }) {
                Leading::Decodable
            } else {
                Leading::Skippable
            }
        }
        _ => Leading::None,
    }
}

/// Source d'AU lisant un fichier TS (voir la documentation du module).
#[derive(Debug)]
pub struct TsAuSource {
    file: File,
    pid: u16,
    codec: Codec,
    locs: Vec<Loc>,
    entries: Vec<FrameEntry>,
    pres_to_decode: Vec<u32>,
}

impl TsAuSource {
    /// Ouvre `path`, lit les tables et indexe toutes les images du premier flux vidéo.
    ///
    /// # Errors
    /// Erreur d'E/S, ou `InvalidData` si le fichier n'est pas un TS exploitable (désynchronisation,
    /// PMT sans flux vidéo, PES sans PTS, PTS en double, plus de 2³² images).
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::open(path)?;
        let (pid, codec, raws) = scan(&file)?;
        let n = u32::try_from(raws.len()).map_err(|_| invalid("trop d'images"))?;
        let mut order: Vec<u32> = (0..n).collect();
        order.sort_by_key(|&d| raws[d as usize].pts);
        if order
            .windows(2)
            .any(|w| raws[w[0] as usize].pts == raws[w[1] as usize].pts)
        {
            return Err(invalid("deux images portent le même PTS"));
        }
        let mut pres = vec![0u32; raws.len()];
        for (p, &d) in (0..n).zip(&order) {
            pres[d as usize] = p;
        }
        let mut rap = None;
        let mut entries = Vec::with_capacity(raws.len());
        for (d, raw) in (0..n).zip(&raws) {
            let pres_idx = pres[d as usize];
            let leading = leading_of(codec, raw, pres_idx, rap);
            if raw.rap != RapKind::None {
                rap = Some((pres_idx, raw.rap));
            }
            entries.push(FrameEntry {
                decode_idx: d,
                pres_idx,
                pts: raw.pts,
                picture: raw.picture,
                rap: raw.rap,
                leading,
            });
        }
        Ok(Self {
            file,
            pid,
            codec,
            locs: raws.iter().map(|r| r.loc).collect(),
            entries,
            pres_to_decode: order,
        })
    }
}

impl AuSource for TsAuSource {
    fn codec(&self) -> Codec {
        self.codec
    }

    fn len(&self) -> u32 {
        u32::try_from(self.entries.len()).unwrap_or(u32::MAX)
    }

    fn entry(&self, decode_idx: u32) -> Option<FrameEntry> {
        self.entries.get(decode_idx as usize).copied()
    }

    fn decode_idx_of(&self, pres_idx: u32) -> Option<u32> {
        self.pres_to_decode.get(pres_idx as usize).copied()
    }

    fn read_au(&self, decode_idx: u32, buf: &mut Vec<u8>) -> io::Result<()> {
        let loc = self.locs.get(decode_idx as usize).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "numéro d'image hors du fichier",
            )
        })?;
        let too_big = || invalid(format!("AU {decode_idx} : plus de {MAX_AU_BYTES} octets"));
        buf.clear();
        let mut pos = loc.offset;
        // Tampon de lecture réutilisé d'un appel à l'autre (les bancs de latence appellent
        // `read_au` en boucle), dimensionné à l'AU et borné à `READ_CHUNK`.
        SCRATCH.with_borrow_mut(|chunk| {
            while pos < loc.end {
                let n = usize::try_from(loc.end - pos).map_or(READ_CHUNK, |r| r.min(READ_CHUNK));
                if chunk.len() < n {
                    chunk.resize(n, 0);
                }
                self.file.read_exact_at(&mut chunk[..n], pos)?;
                for raw in chunk[..n].as_chunks::<TS_PACKET>().0 {
                    let p = parse_packet(raw)?;
                    if p.pid == self.pid {
                        buf.extend_from_slice(p.payload);
                    }
                }
                if buf.len() > MAX_AU_BYTES + PES_SLACK {
                    return Err(too_big());
                }
                pos += n as u64;
            }
            Ok(())
        })?;
        let (es, _) = pes_header(buf)?;
        buf.drain(..es);
        if buf.len() > MAX_AU_BYTES {
            return Err(too_big());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]

    use super::*;
    use ma_decode::Leading::{Decodable, None as N, Skippable};
    use std::sync::atomic::{AtomicU32, Ordering};

    const VIDEO_PID: u16 = 0x100;

    fn packet(pid: u16, pusi: bool, payload: &[u8]) -> Vec<u8> {
        assert!(payload.len() <= 184);
        let mut p = vec![0xFF; TS_PACKET];
        p[0] = 0x47;
        p[1] = (u8::from(pusi) << 6) | (pid >> 8) as u8;
        p[2] = (pid & 0xFF) as u8;
        let pad = 184 - payload.len();
        if pad == 0 {
            p[3] = 0x10;
        } else {
            p[3] = 0x30;
            p[4] = (pad - 1) as u8;
            if pad >= 2 {
                p[5] = 0;
            }
        }
        p[TS_PACKET - payload.len()..].copy_from_slice(payload);
        p
    }

    fn psi(pid: u16, section: &[u8]) -> Vec<u8> {
        let mut payload = vec![0];
        payload.extend_from_slice(section);
        packet(pid, true, &payload)
    }

    fn pat(pmt_pid: u16) -> Vec<u8> {
        let mut s = vec![0x00, 0xB0, 13, 0, 1, 0xC1, 0, 0, 0, 1];
        s.extend_from_slice(&[0xE0 | (pmt_pid >> 8) as u8, (pmt_pid & 0xFF) as u8]);
        s.extend_from_slice(&[0; 4]);
        psi(0, &s)
    }

    /// PMT : flux `(stream_type, pid)`.
    fn pmt(pmt_pid: u16, streams: &[(u8, u16)]) -> Vec<u8> {
        let len = 13 + 5 * streams.len();
        let mut s = vec![
            0x02,
            0xB0 | (len >> 8) as u8,
            (len & 0xFF) as u8,
            0,
            1,
            0xC1,
            0,
            0,
        ];
        s.extend_from_slice(&[0xE1, 0x00, 0xF0, 0x00]);
        for &(t, pid) in streams {
            s.extend_from_slice(&[t, 0xE0 | (pid >> 8) as u8, (pid & 0xFF) as u8, 0xF0, 0]);
        }
        s.extend_from_slice(&[0; 4]);
        psi(pmt_pid, &s)
    }

    fn pts_bytes(pts: i64) -> [u8; 5] {
        [
            0x21 | ((pts >> 29) & 0x0E) as u8,
            (pts >> 22) as u8,
            (((pts >> 15) & 0x7F) << 1) as u8 | 1,
            (pts >> 7) as u8,
            ((pts & 0x7F) << 1) as u8 | 1,
        ]
    }

    fn pes(pts: Option<i64>, es: &[u8]) -> Vec<u8> {
        let mut v = vec![0, 0, 1, 0xE0, 0, 0, 0x80, 0, 0];
        if let Some(pts) = pts {
            v[7] = 0x80;
            v[8] = 5;
            v.extend_from_slice(&pts_bytes(pts));
        }
        v.extend_from_slice(es);
        v
    }

    fn pes_packets(pid: u16, pes: &[u8]) -> Vec<u8> {
        pes.chunks(184)
            .enumerate()
            .flat_map(|(i, c)| packet(pid, i == 0, c))
            .collect()
    }

    struct Tmp(std::path::PathBuf);

    impl Tmp {
        fn new(bytes: &[u8]) -> Self {
            static N: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "ma-testkit-ts-{}-{}.ts",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::write(&path, bytes).unwrap();
            Self(path)
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn stream(stream_type: u8, aus: &[(i64, Vec<u8>)]) -> Vec<u8> {
        let mut v = pat(0x20);
        v.extend(pmt(0x20, &[(0x0F, 0x101), (stream_type, VIDEO_PID)]));
        for (pts, es) in aus {
            v.extend(pes_packets(VIDEO_PID, &pes(Some(*pts), es)));
            // Un paquet d'un autre PID entre deux PES : doit être ignoré.
            v.extend(packet(0x101, true, &[1, 2, 3]));
        }
        v
    }

    fn nal(bytes: &[u8]) -> Vec<u8> {
        let mut v = vec![0, 0, 0, 1];
        v.extend_from_slice(bytes);
        v
    }

    fn h264(kind: char) -> Vec<u8> {
        match kind {
            'I' => nal(&[0x65, 0b1011_0000, 1, 2, 3]),
            'R' => [
                nal(&[0x06, 6, 1, 0b1000_0000, 0x80]),
                nal(&[0x41, 0b1011_0000, 9]),
            ]
            .concat(),
            'P' => nal(&[0x41, 0b1001_1000, 9]),
            _ => nal(&[0x01, 0b1001_1100, 9]),
        }
    }

    const STEP: i64 = 3600;

    fn h264_stream() -> Vec<(i64, Vec<u8>)> {
        // (présentation, type) en ordre de décodage.
        [
            (0, 'I'),
            (3, 'P'),
            (1, 'B'),
            (2, 'B'),
            (6, 'R'),
            (4, 'B'),
            (5, 'B'),
            (9, 'P'),
            (7, 'B'),
            (8, 'B'),
            (12, 'I'),
            (10, 'B'),
            (11, 'B'),
        ]
        .iter()
        .map(|&(p, k)| (p * STEP + 90_000, h264(k)))
        .collect()
    }

    #[test]
    fn leading_status_h264() {
        let t = Tmp::new(&stream(0x1B, &h264_stream()));
        let src = TsAuSource::open(&t.0).unwrap();
        assert_eq!((src.codec(), src.len()), (Codec::H264, 13));
        let lead: Vec<_> = (0..13).map(|d| src.entry(d).unwrap().leading).collect();
        assert_eq!(
            lead,
            [
                N, N, N, N, N, Skippable, Skippable, N, N, N, N, Decodable, Decodable
            ]
        );
        let rap = |d| src.entry(d).unwrap().rap;
        assert_eq!(rap(0), RapKind::Idr);
        assert_eq!(rap(4), RapKind::Recovery { frame_cnt: 0 });
        assert_eq!(rap(1), RapKind::None);
        assert_eq!(src.entry(2).unwrap().picture, PictureType::B);
        // Ordre de présentation.
        assert_eq!(src.decode_idx_of(0), Some(0));
        assert_eq!(src.decode_idx_of(4), Some(5));
        assert_eq!(src.decode_idx_of(13), None);
        assert_eq!(src.entry(13), None);
        assert_eq!(src.entry(5).unwrap().pres_idx, 4);
        assert_eq!(src.entry(5).unwrap().pts, 4 * STEP + 90_000);
    }

    #[test]
    fn read_au_returns_the_elementary_stream() {
        let aus = h264_stream();
        let t = Tmp::new(&stream(0x1B, &aus));
        let src = TsAuSource::open(&t.0).unwrap();
        let mut buf = vec![9; 5];
        for (d, (_, es)) in aus.iter().enumerate() {
            src.read_au(u32::try_from(d).unwrap(), &mut buf).unwrap();
            // Les paquets à remplissage d'adaptation ne laissent que la charge utile.
            assert_eq!(&buf, es, "AU {d}");
        }
        let err = src.read_au(99, &mut buf).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    }

    fn mpeg2_stream(closed: bool) -> Vec<(i64, Vec<u8>)> {
        let seq = nal(&[0xB3, 1, 2, 3]);
        let gop = nal(&[0xB8, 0, 8, 0, if closed { 0x40 } else { 0 }]);
        let pic = |pct: u8| nal(&[0x00, 0, pct << 3, 0xFF]);
        let slice = nal(&[0x01, 0x11]);
        let i = [seq, gop, pic(1), slice.clone()].concat();
        let b = [pic(3), slice.clone()].concat();
        let p = [pic(2), slice].concat();
        // I (présentation 2), B (0), B (1), P (5)
        vec![(2, i), (0, b.clone()), (1, b), (5, p)]
            .into_iter()
            .map(|(p, es)| (p * STEP, es))
            .collect()
    }

    #[test]
    fn leading_status_mpeg2_depends_on_closed_gop() {
        for (closed, expected) in [(false, Leading::Skippable), (true, Leading::Decodable)] {
            let t = Tmp::new(&stream(0x02, &mpeg2_stream(closed)));
            let src = TsAuSource::open(&t.0).unwrap();
            assert_eq!(src.codec(), Codec::Mpeg2);
            assert_eq!(
                src.entry(0).unwrap().rap,
                RapKind::Intra { closed_gop: closed }
            );
            assert_eq!(src.entry(1).unwrap().leading, expected);
            assert_eq!(src.entry(2).unwrap().leading, expected);
            assert_eq!(src.entry(3).unwrap().leading, Leading::None);
            assert_eq!(src.entry(0).unwrap().leading, Leading::None);
        }
    }

    #[test]
    fn hevc_leading_follows_the_nal_type() {
        let slice = |t: u8, bits: u8| nal(&[t << 1, 1, bits]);
        let aus = vec![
            (STEP, slice(19, 0x80)),           // IDR, présentation 1
            (4 * STEP, slice(1, 0b1101_0000)), // TRAIL P, 4
            (3 * STEP, slice(8, 0b1110_0000)), // RASL, 3
            (2 * STEP, slice(6, 0b1110_0000)), // RADL, 2
        ];
        let t = Tmp::new(&stream(0x24, &aus));
        let src = TsAuSource::open(&t.0).unwrap();
        assert_eq!(src.codec(), Codec::Hevc);
        let lead: Vec<_> = (0..4).map(|d| src.entry(d).unwrap().leading).collect();
        assert_eq!(
            lead,
            [
                Leading::None,
                Leading::None,
                Leading::Skippable,
                Leading::Decodable
            ]
        );
        assert_eq!(src.entry(0).unwrap().rap, RapKind::Idr);
    }

    #[test]
    fn pts_roundtrip_on_33_bits() {
        for pts in [0, 1, 90_000, 0x1_2345_6789, (1 << 33) - 1] {
            let (es, got) = pes_header(&pes(Some(pts), &[7])).unwrap();
            assert_eq!((es, got), (14, Some(pts)));
        }
        assert_eq!(pes_header(&pes(None, &[7])).unwrap(), (9, None));
    }

    #[test]
    fn broken_pes_headers_are_rejected() {
        assert!(pes_header(&[0, 0, 2, 0xE0, 0, 0, 0x80, 0, 0]).is_err());
        assert!(pes_header(&[0, 0, 1]).is_err());
        // header_data_length annoncé plus long que le tampon, puis PTS annoncé sans place.
        assert!(pes_header(&[0, 0, 1, 0xE0, 0, 0, 0x80, 0x80, 5, 0]).is_err());
        assert!(pes_header(&[0, 0, 1, 0xE0, 0, 0, 0x80, 0x80, 2, 0, 0]).is_err());
    }

    fn open_err(bytes: &[u8]) -> io::Error {
        TsAuSource::open(&Tmp::new(bytes).0).unwrap_err()
    }

    #[test]
    fn unusable_streams_are_rejected() {
        let good = stream(0x1B, &h264_stream());
        // Désynchronisation.
        let mut bad = good.clone();
        bad[TS_PACKET * 3] = 0;
        assert_eq!(open_err(&bad).kind(), io::ErrorKind::InvalidData);
        // Fichier sans PMT.
        assert!(open_err(&pat(0x20)).to_string().contains("PMT"));
        // PMT sans flux vidéo.
        let mut audio = pat(0x20);
        audio.extend(pmt(0x20, &[(0x0F, 0x101)]));
        assert!(open_err(&audio).to_string().contains("aucun flux vidéo"));
        // PES sans PTS.
        let mut no_pts = pat(0x20);
        no_pts.extend(pmt(0x20, &[(0x1B, VIDEO_PID)]));
        no_pts.extend(pes_packets(VIDEO_PID, &pes(None, &h264('I'))));
        assert!(open_err(&no_pts).to_string().contains("sans PTS"));
        // PTS en double.
        let dup = stream(0x1B, &[(5, h264('I')), (5, h264('P'))]);
        assert!(open_err(&dup).to_string().contains("même PTS"));
        // Fichier absent.
        assert_eq!(
            TsAuSource::open(Path::new("/nonexistent.ts"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::NotFound
        );
    }

    #[test]
    fn broken_tables_are_rejected() {
        // PAT sans programme (seulement le programme 0 = NIT).
        let mut s = vec![0x00, 0xB0, 13, 0, 1, 0xC1, 0, 0, 0, 0, 0xE0, 0x10];
        s.extend_from_slice(&[0; 4]);
        assert!(open_err(&psi(0, &s)).to_string().contains("sans programme"));
        // Section annoncée plus longue que le paquet.
        let long = [0x00, 0xB1, 0xFF, 0, 1, 0xC1, 0, 0];
        assert!(open_err(&psi(0, &long)).to_string().contains("PSI"));
        // Section trop courte, table de mauvais type, pointeur hors paquet, paquet sans charge.
        assert!(
            open_err(&psi(0, &[0x00, 0xB0, 2, 0, 1]))
                .to_string()
                .contains("PSI")
        );
        assert!(
            open_err(&psi(
                0,
                &[
                    0x42, 0xB0, 13, 0, 1, 0xC1, 0, 0, 0, 1, 0xE0, 0x20, 0, 0, 0, 0
                ]
            ))
            .to_string()
            .contains("attendue")
        );
        let mut ptr = packet(0, true, &[200, 1]);
        assert!(open_err(&ptr).to_string().contains("PSI"));
        ptr[3] = 0x20; // adaptation seule, aucune charge utile
        assert!(open_err(&ptr).to_string().contains("PSI"));
        // PMT dont la section est trop courte pour contenir program_info_length.
        for len in [9u8, 10] {
            let mut m = vec![0x02, 0xB0, len, 0, 1, 0xC1, 0, 0, 0xE1, 0, 0xF0];
            m.truncate(3 + usize::from(len) - 4);
            m.extend_from_slice(&[0; 4]);
            let mut bytes = pat(0x20);
            bytes.extend(psi(0x20, &m));
            assert!(
                open_err(&bytes).to_string().contains("PMT"),
                "section_length {len}"
            );
        }
        // PMT dont une description de flux dépasse la section.
        let mut m = vec![0x02, 0xB0, 18, 0, 1, 0xC1, 0, 0, 0xE1, 0, 0xF0, 0];
        m.extend_from_slice(&[0x0F, 0xE1, 1, 0xF0, 0x40]);
        m.extend_from_slice(&[0; 4]);
        let mut bytes = pat(0x20);
        bytes.extend(psi(0x20, &m));
        assert!(open_err(&bytes).to_string().contains("PMT"));
        // program_info_length dépassant la section.
        let mut m = vec![0x02, 0xB0, 13, 0, 1, 0xC1, 0, 0, 0xE1, 0, 0xF0, 0xFF];
        m.extend_from_slice(&[0; 4]);
        let mut bytes = pat(0x20);
        bytes.extend(psi(0x20, &m));
        assert!(open_err(&bytes).to_string().contains("PMT"));
    }

    #[test]
    fn trailing_partial_packet_and_leading_continuation_are_ignored() {
        let mut bytes = pat(0x20);
        bytes.extend(pmt(0x20, &[(0x1B, VIDEO_PID)]));
        // Fin de PES dont le début a été coupé : aucune AU ouverte.
        bytes.extend(packet(VIDEO_PID, false, &[1, 2, 3]));
        bytes.extend(pes_packets(VIDEO_PID, &pes(Some(0), &h264('I'))));
        bytes.extend_from_slice(&[0x47, 0, 0]); // paquet incomplet en fin de fichier
        let src = TsAuSource::open(&Tmp::new(&bytes).0).unwrap();
        assert_eq!(src.len(), 1);
        assert!(!src.is_empty());
    }

    #[test]
    fn large_aus_are_read_and_bounded() {
        let mut big_es = nal(&[0x65, 0b1011_0000]);
        big_es.resize(MAX_AU_BYTES, 0xAB);
        let at_limit = Tmp::new(&stream(0x1B, &[(0, big_es.clone())]));
        let src = TsAuSource::open(&at_limit.0).unwrap();
        let mut buf = Vec::new();
        src.read_au(0, &mut buf).unwrap();
        assert_eq!(buf.len(), MAX_AU_BYTES);
        assert_eq!(buf[..6], big_es[..6]);

        big_es.resize(MAX_AU_BYTES + 1, 0xAB);
        let over = Tmp::new(&stream(0x1B, &[(0, big_es.clone())]));
        let err = TsAuSource::open(&over.0)
            .unwrap()
            .read_au(0, &mut buf)
            .unwrap_err();
        assert!(err.to_string().contains("plus de"));
        // Bien au-delà : arrêt avant la fin de l'AU.
        big_es.resize(MAX_AU_BYTES + 4096, 0xAB);
        let way_over = Tmp::new(&stream(0x1B, &[(0, big_es)]));
        let err = TsAuSource::open(&way_over.0)
            .unwrap()
            .read_au(0, &mut buf)
            .unwrap_err();
        assert!(err.to_string().contains("plus de"));
    }
}
