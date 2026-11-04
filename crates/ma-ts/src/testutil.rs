//! Outils de test : construction de paquets et re-paquetage (feature `testutil`, utilisée aussi
//! par `xtask` pour fabriquer les fichiers dérivés du corpus).

// Champs de bits écrits octet par octet : valeurs masquées ou bornées avant la conversion.
#![allow(clippy::cast_possible_truncation)]

use ma_core::Pcr;

use crate::packet::{PacketFormat, SYNC_BYTE, TS_PACKET_SIZE, parse_packet};
use crate::psi::{Section, SectionAssembler, crc32_mpeg2};
use crate::scan::ScannedPacket;

/// Champ d'adaptation à écrire.
#[derive(Debug, Clone, Copy, Default)]
pub struct AfSpec {
    pub discontinuity: bool,
    pub random_access: bool,
    pub pcr: Option<Pcr>,
}

/// Construit un paquet de 188 octets. Sans champ d'adaptation, la charge utile est complétée par
/// des 0xFF ; avec, le champ d'adaptation est bourré pour que la charge utile finisse le paquet
/// (une charge trop longue est tronquée).
#[must_use]
pub fn packet(pid: u16, pusi: bool, cc: u8, af: Option<AfSpec>, payload: &[u8]) -> [u8; 188] {
    let mut p = [0xFFu8; TS_PACKET_SIZE];
    p[0] = SYNC_BYTE;
    p[1] = u8::from(pusi) << 6 | ((pid >> 8) & 0x1F) as u8;
    p[2] = (pid & 0xFF) as u8;
    let Some(af) = af else {
        p[3] = 0x10 | (cc & 0x0F);
        let n = payload.len().min(TS_PACKET_SIZE - 4);
        p[4..4 + n].copy_from_slice(&payload[..n]);
        return p;
    };
    let needed = if af.pcr.is_some() { 7 } else { 1 };
    let n = payload.len().min(TS_PACKET_SIZE - 5 - needed);
    let af_len = TS_PACKET_SIZE - 5 - n;
    p[3] = if n == 0 { 0x20 } else { 0x30 } | (cc & 0x0F);
    p[4] = af_len as u8;
    p[5] = u8::from(af.discontinuity) << 7
        | u8::from(af.random_access) << 6
        | u8::from(af.pcr.is_some()) << 4;
    if let Some(pcr) = af.pcr {
        let base = pcr.base & ((1 << 33) - 1);
        p[6] = (base >> 25) as u8;
        p[7] = (base >> 17) as u8;
        p[8] = (base >> 9) as u8;
        p[9] = (base >> 1) as u8;
        p[10] = ((base & 1) as u8) << 7 | 0x7E | ((pcr.ext >> 8) & 1) as u8;
        p[11] = (pcr.ext & 0xFF) as u8;
    }
    p[5 + af_len..].copy_from_slice(&payload[..n]);
    p
}

/// Section longue (`ssi = 1`, `current_next = 1`, section 0/0) avec son CRC-32.
///
/// # Panics
/// Si `body` dépasse la capacité d'une section (1012 octets pour une PSI, ici bornée à 4084).
#[must_use]
pub fn section(table_id: u8, ext: u16, version: u8, body: &[u8]) -> Vec<u8> {
    section_numbered(table_id, ext, version, (0, 0), body)
}

/// Comme [`section`], avec `(section_number, last_section_number)`.
///
/// # Panics
/// Si `body` est trop long.
#[must_use]
pub fn section_numbered(
    table_id: u8,
    ext: u16,
    version: u8,
    (number, last): (u8, u8),
    body: &[u8],
) -> Vec<u8> {
    let length = body.len() + 5 + 4;
    assert!(length <= 4093, "section trop longue");
    let mut s = vec![
        table_id,
        0xB0 | ((length >> 8) & 0x0F) as u8,
        (length & 0xFF) as u8,
        (ext >> 8) as u8,
        (ext & 0xFF) as u8,
        0xC0 | (version & 0x1F) << 1 | 1,
        number,
        last,
    ];
    s.extend_from_slice(body);
    s.extend(crc32_mpeg2(&s).to_be_bytes());
    s
}

/// Paquets portant `section` sur `pid` : `pointer_field` 0, bourrage 0xFF, compteurs à partir
/// de `cc`.
#[must_use]
pub fn psi_packets(pid: u16, section: &[u8], cc: u8) -> Vec<[u8; 188]> {
    let mut out = Vec::new();
    let mut first = vec![0u8];
    let n = section.len().min(183);
    first.extend_from_slice(&section[..n]);
    out.push(packet(pid, true, cc, None, &first));
    for (k, chunk) in section[n..].chunks(184).enumerate() {
        out.push(packet(
            pid,
            false,
            cc.wrapping_add(k as u8).wrapping_add(1),
            None,
            chunk,
        ));
    }
    out
}

/// Section décodée par un assembleur (pour tester les analyseurs de tables).
///
/// # Panics
/// Si `bytes` n'est pas une section complète.
#[must_use]
pub fn section_of(bytes: &[u8]) -> Section {
    let mut asm = SectionAssembler::new(0x30);
    let mut out = Vec::new();
    let mut sink = ma_core::VecSink::default();
    for (i, raw) in psi_packets(0x30, bytes, 0).iter().enumerate() {
        let packet = parse_packet(raw).expect("paquet valide");
        let sp = ScannedPacket {
            offset: i as u64 * 188,
            index: i as u64,
            packet,
        };
        asm.push(&sp, &mut out, &mut sink);
    }
    out.into_iter().next().expect("section complète")
}

/// Descripteur `tag`, longueur, données.
#[must_use]
pub fn descriptor(tag: u8, data: &[u8]) -> Vec<u8> {
    let mut d = vec![tag, data.len() as u8];
    d.extend_from_slice(data);
    d
}

/// Section PAT (le programme 0 n'est pas écrit).
#[must_use]
pub fn pat(ts_id: u16, version: u8, programs: &[(u16, u16)]) -> Vec<u8> {
    let body: Vec<u8> = programs
        .iter()
        .flat_map(|&(n, pid)| [(n >> 8) as u8, n as u8, 0xE0 | (pid >> 8) as u8, pid as u8])
        .collect();
    section(0x00, ts_id, version, &body)
}

/// Section de PAT numérotée (`number` sur `last`).
#[must_use]
pub fn pat_section(
    ts_id: u16,
    version: u8,
    number: u8,
    last: u8,
    programs: &[(u16, u16)],
) -> Vec<u8> {
    let body: Vec<u8> = programs
        .iter()
        .flat_map(|&(n, pid)| [(n >> 8) as u8, n as u8, 0xE0 | (pid >> 8) as u8, pid as u8])
        .collect();
    section_numbered(0x00, ts_id, version, (number, last), &body)
}

/// Section PMT : `program_info` et les descripteurs de chaque flux sont des boucles déjà
/// encodées (voir [`descriptor`]).
#[must_use]
pub fn pmt(
    program: u16,
    version: u8,
    pcr_pid: u16,
    program_info: &[u8],
    streams: &[(u8, u16, &[u8])],
) -> Vec<u8> {
    let mut body = vec![
        0xE0 | (pcr_pid >> 8) as u8,
        pcr_pid as u8,
        0xF0 | (program_info.len() >> 8) as u8,
        program_info.len() as u8,
    ];
    body.extend_from_slice(program_info);
    for &(stream_type, pid, info) in streams {
        body.extend([
            stream_type,
            0xE0 | (pid >> 8) as u8,
            pid as u8,
            0xF0 | (info.len() >> 8) as u8,
            info.len() as u8,
        ]);
        body.extend_from_slice(info);
    }
    section(0x02, program, version, &body)
}

/// Texte DVB : ASCII tel quel, sinon préfixe 0x15 et UTF-8 (comme le muxeur de `FFmpeg`).
fn dvb_text(t: &str) -> Vec<u8> {
    let mut v = Vec::new();
    if !t.is_ascii() {
        v.push(0x15);
    }
    v.extend_from_slice(t.as_bytes());
    v
}

/// Section SDT du flux courant : (`service_id`, fournisseur, nom).
#[must_use]
pub fn sdt(ts_id: u16, version: u8, services: &[(u16, &str, &str)]) -> Vec<u8> {
    let mut body = vec![0xFF, 0x01, 0xFF];
    for &(id, provider, name) in services {
        let (p, n) = (dvb_text(provider), dvb_text(name));
        let mut d = vec![0x01, p.len() as u8];
        d.extend(&p);
        d.push(n.len() as u8);
        d.extend(&n);
        let desc = descriptor(0x48, &d);
        body.extend([(id >> 8) as u8, id as u8, 0xFC]);
        // running_status 4, free_CA 0, descriptors_loop_length.
        body.extend([0x80 | (desc.len() >> 8) as u8, desc.len() as u8]);
        body.extend(desc);
    }
    section(0x42, ts_id, version, &body)
}

fn pid_of(p: &[u8]) -> u16 {
    u16::from(p[1] & 0x1F) << 8 | u16::from(p[2])
}

fn is_null(p: &[u8]) -> bool {
    p[0] == SYNC_BYTE && pid_of(p) == 0x1FFF
}

/// Remplace chaque occurrence de table du `pid` (un paquet à PUSI et ses suites) par `sections`.
///
/// Stratégie : les paquets de l'occurrence sont réutilisés dans l'ordre ; les paquets
/// supplémentaires nécessaires prennent les premiers paquets nuls (PID 0x1FFF) qui suivent
/// (dans les 64 paquets), les paquets inutilisés deviennent nuls. Le nombre de paquets du flux
/// et leurs positions sont conservés (donc le timing d'un flux à débit constant). Les compteurs
/// de continuité du `pid` sont ensuite renumérotés.
///
/// # Errors
/// Si une occurrence ne trouve pas assez de paquets nuls à proximité.
pub fn replace_psi(ts: &[u8], pid: u16, sections: &[Vec<u8>]) -> Result<Vec<u8>, &'static str> {
    const WINDOW: usize = 64;
    let mut out = ts.to_vec();
    let count = out.len() / TS_PACKET_SIZE;
    let range = |index: usize| index * TS_PACKET_SIZE..(index + 1) * TS_PACKET_SIZE;
    // Début (`pusi`) ou suite d'une table du `pid`.
    let table_packet = |out: &[u8], index: usize, start: bool| {
        let raw = &out[range(index)];
        raw[0] == SYNC_BYTE && pid_of(raw) == pid && (raw[1] & 0x40 != 0) == start
    };
    let new: Vec<[u8; 188]> = sections
        .iter()
        .flat_map(|section| psi_packets(pid, section, 0))
        .collect();
    let mut cursor = 0;
    while cursor < count {
        if !table_packet(&out, cursor, true) {
            cursor += 1;
            continue;
        }
        // Emplacements de l'occurrence : son premier paquet et ses suites avant la suivante.
        let mut slots = vec![cursor];
        let mut look = cursor + 1;
        while look < count && look < cursor + WINDOW && !table_packet(&out, look, true) {
            if table_packet(&out, look, false) {
                slots.push(look);
            }
            look += 1;
        }
        let last = slots.last().copied().unwrap_or(cursor);
        // Complète avec les premiers paquets nuls qui suivent.
        let mut scan = last + 1;
        while slots.len() < new.len() {
            if scan >= count || scan > last + WINDOW {
                return Err("pas assez de paquets nuls pour la table réécrite");
            }
            if is_null(&out[range(scan)]) {
                slots.push(scan);
            }
            scan += 1;
        }
        let end = slots.last().copied().unwrap_or(last);
        for (rank, slot) in slots.iter().enumerate() {
            let replacement = new
                .get(rank)
                .copied()
                .unwrap_or_else(|| packet(0x1FFF, false, 0, None, &[]));
            out[range(*slot)].copy_from_slice(&replacement);
        }
        cursor = end + 1;
    }
    // Renumérote les compteurs du `pid`.
    let mut cc = 0u8;
    for index in 0..count {
        let raw = &mut out[range(index)];
        if raw[0] == SYNC_BYTE && pid_of(raw) == pid {
            raw[3] = raw[3] & 0xF0 | cc;
            if raw[3] & 0x10 != 0 {
                cc = (cc + 1) & 0x0F;
            }
        }
    }
    Ok(out)
}

/// Remplace des paquets nuls (PID 0x1FFF) par des paquets construits à la demande : pour chaque
/// position cible (indice de paquet), le premier paquet nul à cette position ou après est
/// remplacé par `make(position réelle, rang de la cible)`. Les positions cibles doivent être
/// croissantes. Le nombre de paquets et le timing du flux sont conservés.
///
/// # Errors
/// Si aucun paquet nul n'est disponible pour une cible.
pub fn fill_nulls(
    ts: &[u8],
    targets: &[usize],
    mut make: impl FnMut(usize, usize) -> [u8; 188],
) -> Result<Vec<u8>, &'static str> {
    let mut out = ts.to_vec();
    let count = out.len() / TS_PACKET_SIZE;
    let mut floor = 0;
    for (rank, &target) in targets.iter().enumerate() {
        let mut at = target.max(floor);
        while at < count && !is_null(&out[at * TS_PACKET_SIZE..(at + 1) * TS_PACKET_SIZE]) {
            at += 1;
        }
        if at >= count {
            return Err("aucun paquet nul disponible");
        }
        out[at * TS_PACKET_SIZE..(at + 1) * TS_PACKET_SIZE].copy_from_slice(&make(at, rank));
        floor = at + 1;
    }
    Ok(out)
}

/// PTS ou DTS sur 5 octets (`prefix` : 2 = PTS seul, 3 = PTS avant DTS, 1 = DTS).
fn timestamp_bytes(prefix: u8, ts: u64) -> [u8; 5] {
    let ts = ts & ((1 << 33) - 1);
    [
        prefix << 4 | ((ts >> 29) & 0x0E) as u8 | 1,
        (ts >> 22) as u8,
        ((ts >> 14) & 0xFE) as u8 | 1,
        (ts >> 7) as u8,
        ((ts << 1) & 0xFE) as u8 | 1,
    ]
}

/// En-tête PES d'un flux vidéo (`stream_id` 0xE0) : `packet_length` déclaré, PTS et DTS facultatifs
/// (un DTS sans PTS est ignoré) et `stuffing` octets 0xFF de bourrage dans l'en-tête.
///
/// # Panics
/// Si l'en-tête dépasse 255 octets de données facultatives.
#[must_use]
pub fn pes_header(
    packet_length: u16,
    pts: Option<u64>,
    dts: Option<u64>,
    stuffing: usize,
) -> Vec<u8> {
    let dts = dts.filter(|_| pts.is_some());
    let mut optional = Vec::new();
    if let Some(pts) = pts {
        optional.extend(timestamp_bytes(if dts.is_some() { 3 } else { 2 }, pts));
    }
    if let Some(dts) = dts {
        optional.extend(timestamp_bytes(1, dts));
    }
    optional.resize(optional.len() + stuffing, 0xFF);
    let flags = match (pts, dts) {
        (Some(_), Some(_)) => 0xC0,
        (Some(_), None) => 0x80,
        _ => 0,
    };
    let mut h = vec![
        0,
        0,
        1,
        0xE0,
        (packet_length >> 8) as u8,
        packet_length as u8,
        0x80,
        flags,
    ];
    h.push(u8::try_from(optional.len()).expect("en-tête PES trop long"));
    h.extend(optional);
    h
}

/// PES complet : en-tête (`packet_length` renseigné s'il tient sur 16 bits, sinon 0) et charge.
#[must_use]
pub fn pes_bytes(pts: Option<u64>, dts: Option<u64>, payload: &[u8]) -> Vec<u8> {
    let probe = pes_header(0, pts, dts, 0);
    let length = u16::try_from(probe.len() - 6 + payload.len()).unwrap_or(0);
    let mut pes = pes_header(length, pts, dts, 0);
    pes.extend_from_slice(payload);
    pes
}

/// Champ d'adaptation conservé d'un paquet vidéo d'origine : drapeaux et PCR bruts.
#[derive(Debug, Clone, Copy)]
struct KeptAf {
    flags: u8,
    pcr: [u8; 6],
}

impl KeptAf {
    /// Octets du champ d'adaptation conservé, hors octet de longueur (drapeaux + PCR).
    const LEN: usize = 7;
}

/// Paquet vidéo : `payload` finit le paquet (bourrage dans le champ d'adaptation), qui reprend
/// `keep` (drapeaux et PCR) s'il y en a un.
fn video_packet(pid: u16, pusi: bool, cc: u8, keep: Option<KeptAf>, payload: &[u8]) -> [u8; 188] {
    let mut p = [0xFFu8; TS_PACKET_SIZE];
    p[0] = SYNC_BYTE;
    p[1] = u8::from(pusi) << 6 | ((pid >> 8) & 0x1F) as u8;
    p[2] = (pid & 0xFF) as u8;
    let max = TS_PACKET_SIZE - 4 - keep.map_or(0, |_| 1 + KeptAf::LEN);
    let n = payload.len().min(max);
    if keep.is_none() && n == TS_PACKET_SIZE - 4 {
        p[3] = 0x10 | (cc & 0x0F);
        p[4..].copy_from_slice(&payload[..n]);
        return p;
    }
    let af_len = TS_PACKET_SIZE - 5 - n;
    p[3] = 0x30 | (cc & 0x0F);
    p[4] = af_len as u8;
    if let Some(k) = keep {
        p[5] = k.flags;
        p[6..12].copy_from_slice(&k.pcr);
    } else if af_len > 0 {
        p[5] = 0;
    }
    p[5 + af_len..].copy_from_slice(&payload[..n]);
    p
}

/// Paquets portant `pes` sur `pid` (début de PES au premier paquet, compteurs à partir de `cc`,
/// bourrage dans le champ d'adaptation du dernier paquet). Le premier paquet porte `first_len`
/// octets de PES au plus (184 au maximum), les suivants 184.
#[must_use]
pub fn pes_packets(pid: u16, cc: u8, pes: &[u8], first_len: usize) -> Vec<[u8; 188]> {
    let first_len = first_len.min(184).min(pes.len());
    let (head, tail) = pes.split_at(first_len);
    let mut out = vec![video_packet(pid, true, cc, None, head)];
    for (k, c) in tail.chunks(184).enumerate() {
        out.push(video_packet(
            pid,
            false,
            cc.wrapping_add(k as u8).wrapping_add(1),
            None,
            c,
        ));
    }
    out
}

/// PES d'origine d'un flux vidéo, tel que lu par [`rewrite_video_pes`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrigPes {
    pub pts: Option<u64>,
    pub dts: Option<u64>,
    /// Octets d'ES (sans l'en-tête PES).
    pub payload: Vec<u8>,
}

/// PES à écrire par [`rewrite_video_pes`].
pub type NewPes = OrigPes;

/// Réécrit les PES du PID vidéo `pid` : `f` reçoit les PES d'origine et renvoie les nouveaux.
///
/// Les paquets vidéo sont réécrits aux positions d'origine, dans l'ordre, en gardant le champ
/// d'adaptation des paquets qui portent un PCR ; les paquets en surplus deviennent des paquets
/// nuls, ceux qui manquent sont insérés après le dernier paquet vidéo. Les compteurs de
/// continuité de `pid` sont renumérotés. Les paquets des autres PID sont conservés tels quels.
///
/// # Panics
/// Si `ts` contient des paquets de `pid` sans en-tête PES valide, ou si un PES dépasse la
/// capacité d'un en-tête.
#[must_use]
pub fn rewrite_video_pes(
    ts: &[u8],
    pid: u16,
    f: impl FnOnce(Vec<OrigPes>) -> Vec<NewPes>,
) -> Vec<u8> {
    let (raw, _) = ts.as_chunks::<TS_PACKET_SIZE>();
    // Paquets vidéo : indice, champ d'adaptation conservé ; PES d'origine accumulés.
    let mut slots: Vec<(usize, Option<KeptAf>)> = Vec::new();
    let mut pes_raw: Vec<Vec<u8>> = Vec::new();
    for (i, p) in raw.iter().enumerate() {
        if p[0] != SYNC_BYTE || pid_of(p) != pid {
            continue;
        }
        let packet = parse_packet(p).expect("paquet vidéo invalide");
        let keep = packet.adaptation.filter(|a| a.pcr.is_some()).map(|_| {
            let mut pcr = [0u8; 6];
            pcr.copy_from_slice(&p[6..12]);
            KeptAf { flags: p[5], pcr }
        });
        slots.push((i, keep));
        if packet.header.pusi {
            pes_raw.push(Vec::new());
        }
        if let Some(cur) = pes_raw.last_mut() {
            cur.extend_from_slice(packet.payload);
        }
    }
    let orig: Vec<OrigPes> = pes_raw
        .iter()
        .map(|bytes| {
            let h = crate::pes::parse_pes_header(bytes).expect("en-tête PES d'origine invalide");
            OrigPes {
                pts: h.pts,
                dts: h.dts,
                payload: bytes[h.header_len..].to_vec(),
            }
        })
        .collect();
    let new = f(orig);
    // Paquets à écrire, dans l'ordre : chaque PES prend les emplacements suivants.
    let mut written: Vec<[u8; 188]> = Vec::new();
    let mut cc = 0u8;
    let mut next_slot = 0;
    for pes in &new {
        let bytes = pes_bytes(pes.pts, pes.dts, &pes.payload);
        let mut rest = bytes.as_slice();
        let mut first = true;
        while !rest.is_empty() {
            let keep = slots.get(next_slot).and_then(|s| s.1);
            next_slot += 1;
            let cap = TS_PACKET_SIZE - 4 - keep.map_or(0, |_| 1 + KeptAf::LEN);
            let (now, later) = rest.split_at(rest.len().min(cap));
            written.push(video_packet(pid, first, cc, keep, now));
            cc = (cc + 1) & 0x0F;
            first = false;
            rest = later;
        }
    }
    let null = packet(0x1FFF, false, 0, None, &[]);
    let mut out = Vec::with_capacity(ts.len());
    let last_slot = slots.last().map(|s| s.0);
    let mut slot_rank = 0;
    for (i, p) in raw.iter().enumerate() {
        if p[0] == SYNC_BYTE && pid_of(p) == pid {
            out.extend(written.get(slot_rank).unwrap_or(&null));
            slot_rank += 1;
        } else {
            out.extend(p);
        }
        if Some(i) == last_slot {
            for extra in written.iter().skip(slots.len()) {
                out.extend(extra);
            }
        }
    }
    out
}

/// Réécrit un flux de paquets de 188 octets dans le format `f` (le reste incomplet est ignoré) :
/// 204 et 208 ajoutent 16 et 20 octets 0x00 ; TTS ajoute un préfixe de 4 octets (compteur 27 MHz
/// croissant, big-endian).
#[must_use]
pub fn repacketize(ts: &[u8], f: PacketFormat) -> Vec<u8> {
    let (packets, _) = ts.as_chunks::<TS_PACKET_SIZE>();
    let mut out = Vec::with_capacity(packets.len() * f.size());
    for (i, p) in packets.iter().enumerate() {
        if f == PacketFormat::Tts192 {
            let ticks = (i as u32).wrapping_mul(1000) & 0x3FFF_FFFF;
            out.extend(ticks.to_be_bytes());
        }
        out.extend(p);
        out.resize(out.len() + f.size() - f.sync_offset() - TS_PACKET_SIZE, 0);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::parse_packet;

    #[test]
    fn packet_is_188_bytes_with_sync_and_padding() {
        let p = packet(0x100, false, 3, None, &[1]);
        assert_eq!(p.len(), 188);
        assert_eq!((p[0], p[4], p[5], p[187]), (0x47, 1, 0xFF, 0xFF));
        // Charge utile trop longue : tronquée, jamais de panique.
        let p = packet(0x100, false, 3, Some(AfSpec::default()), &[7; 400]);
        assert_eq!(p[0], 0x47);
        let p = packet(
            0x100,
            false,
            3,
            Some(AfSpec {
                pcr: Some(ma_core::Pcr { base: 1, ext: 2 }),
                ..AfSpec::default()
            }),
            &[7; 400],
        );
        assert_eq!(p[4], 7);
    }

    #[test]
    fn repacketize_sizes_and_tts_counter() {
        let ts: Vec<u8> = (0..3)
            .flat_map(|i| packet(0x100, false, i, None, &[]))
            .collect();
        assert_eq!(repacketize(&ts, PacketFormat::Ts188), ts);
        let r = repacketize(&ts, PacketFormat::Rs204);
        assert_eq!(r.len(), 3 * 204);
        assert!(r[188..204].iter().all(|&b| b == 0));
        assert_eq!(repacketize(&ts, PacketFormat::Ts208).len(), 3 * 208);
        let t = repacketize(&ts, PacketFormat::Tts192);
        assert_eq!(t.len(), 3 * 192);
        assert_eq!(t[4], 0x47);
        let c = |i: usize| u32::from_be_bytes(t[i * 192..i * 192 + 4].try_into().unwrap());
        assert!(c(0) < c(1) && c(1) < c(2));
        // Reste incomplet ignoré.
        assert_eq!(repacketize(&ts[..400], PacketFormat::Rs204).len(), 2 * 204);
    }

    /// Flux : [PMT, vidéo, nuls, vidéo, PMT, vidéo] (PID 0x1000 pour la PMT).
    fn small_stream(nulls: usize) -> Vec<u8> {
        let table = section(0x02, 1, 0, &[1, 2, 3]);
        let mut ts = Vec::new();
        let mut push = |p: [u8; 188]| ts.extend(p);
        push(psi_packets(0x1000, &table, 5)[0]);
        push(packet(0x100, false, 0, None, &[1]));
        for _ in 0..nulls {
            push(packet(0x1FFF, false, 0, None, &[]));
        }
        push(packet(0x100, false, 1, None, &[2]));
        push(psi_packets(0x1000, &table, 9)[0]);
        push(packet(0x100, false, 2, None, &[3]));
        for _ in 0..nulls {
            push(packet(0x1FFF, false, 0, None, &[]));
        }
        ts
    }

    fn pids(ts: &[u8]) -> Vec<u16> {
        ts.chunks(188).map(pid_of).collect()
    }

    #[test]
    fn replace_psi_uses_neighbouring_nulls_and_renumbers() {
        let ts = small_stream(1);
        // Table de 2 paquets (corps de 300 octets) : un paquet nul voisin est consommé.
        let long = section(0x02, 1, 1, &[7; 300]);
        let out = replace_psi(&ts, 0x1000, std::slice::from_ref(&long)).unwrap();
        assert_eq!(out.len(), ts.len());
        assert_eq!(
            pids(&out),
            vec![0x1000, 0x100, 0x1000, 0x100, 0x1000, 0x100, 0x1000]
        );
        // Les compteurs du PID sont continus.
        let ccs: Vec<u8> = out
            .chunks(188)
            .filter(|p| pid_of(p) == 0x1000)
            .map(|p| p[3] & 0x0F)
            .collect();
        assert_eq!(ccs, vec![0, 1, 2, 3]);
        // La table se relit : 1re occurrence = paquets 0 et 2.
        let section = section_of_packets(&[&out[..188], &out[188 * 2..188 * 3]]);
        assert_eq!(section, long);
        // Table plus courte que l'original sur plusieurs paquets : les restes deviennent nuls.
        let out = replace_psi(&out, 0x1000, &[section_of_short()]).unwrap();
        assert_eq!(out.len(), ts.len());
        assert_eq!(pids(&out).iter().filter(|&&p| p == 0x1000).count(), 2);
    }

    fn section_of_short() -> Vec<u8> {
        section(0x02, 1, 2, &[9])
    }

    /// Octets de section portés par une suite de paquets (`pointer_field` ignoré).
    fn section_of_packets(packets: &[&[u8]]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for (k, p) in packets.iter().enumerate() {
            bytes.extend_from_slice(&p[if k == 0 { 5 } else { 4 }..]);
        }
        let length = usize::from(bytes[1] & 0x0F) << 8 | usize::from(bytes[2]);
        bytes.truncate(3 + length);
        bytes
    }

    #[test]
    fn replace_psi_without_nulls_fails() {
        let ts = small_stream(0);
        let long = section(0x02, 1, 1, &[7; 300]);
        assert!(replace_psi(&ts, 0x1000, &[long]).is_err());
        // Même sans paquets nuls, une table de même longueur est remplacée en place.
        let same = section(0x02, 1, 1, &[4, 5, 6]);
        assert!(replace_psi(&ts, 0x1000, &[same]).is_ok());
    }

    /// Flux : audio, puis 3 PES vidéo (500 octets d'ES chacun, 3 paquets, PCR sur chaque
    /// premier paquet), un paquet audio entre chaque PES.
    fn video_stream() -> Vec<u8> {
        let mut ts = Vec::new();
        ts.extend(packet(0x300, true, 0, None, &[1; 20]));
        let mut cc = 0;
        for k in 0..3u8 {
            let pes = pes_bytes(Some(u64::from(k) * 3600), None, &[k; 500]);
            let mut packets = pes_packets(0x100, cc, &pes, 184);
            // PCR sur le premier paquet : charge réduite, donc PES un peu plus long en paquets.
            packets[0] = packet(
                0x100,
                true,
                cc,
                Some(AfSpec {
                    pcr: Some(Pcr {
                        base: u64::from(k) + 10,
                        ext: 1,
                    }),
                    random_access: true,
                    ..AfSpec::default()
                }),
                &pes[..176],
            );
            let rest = pes_packets(0x100, cc + 1, &pes[176..], 184);
            packets.truncate(1);
            packets.extend(rest.into_iter().map(|mut p| {
                p[1] &= !0x40;
                p
            }));
            cc += packets.len() as u8;
            ts.extend(packets.iter().flatten());
            ts.extend(packet(0x300, false, k + 1, None, &[2; 20]));
        }
        ts
    }

    fn video_of(ts: &[u8]) -> Vec<(usize, [u8; 188])> {
        ts.chunks(188)
            .enumerate()
            .filter(|(_, p)| pid_of(p) == 0x100)
            .map(|(i, p)| (i, p.try_into().unwrap()))
            .collect()
    }

    #[test]
    fn rewrite_video_pes_keeps_positions_pcr_and_pads_with_nulls() {
        let ts = video_stream();
        let before = video_of(&ts);
        assert_eq!(before.len(), 9);
        // Fusionne les 2 premiers PES en un seul et supprime le 3e : moins de paquets, le reste devient nul.
        let mut seen = Vec::new();
        let out = rewrite_video_pes(&ts, 0x100, |orig| {
            seen = orig.clone();
            vec![NewPes {
                pts: Some(7),
                dts: None,
                payload: orig
                    .iter()
                    .take(2)
                    .flat_map(|o| o.payload.clone())
                    .collect(),
            }]
        });
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[1].pts, Some(3600));
        assert_eq!(seen[2].payload, vec![2; 500]);
        assert_eq!(out.len(), ts.len(), "même nombre de paquets");
        let after = video_of(&out);
        // 1000 octets d'ES + 14 d'en-tête tiennent en 6 paquets (deux portent un PCR) ; les autres
        // emplacements sont nuls, les paquets des autres PID sont intacts.
        assert_eq!(after.len(), 6);
        assert!(after.len() < before.len(), "{} paquets vidéo", after.len());
        let nulls = out.chunks(188).filter(|p| pid_of(p) == 0x1FFF).count();
        assert_eq!(after.len() + nulls, before.len());
        assert_eq!(
            after.iter().filter(|p| p.1[1] & 0x40 != 0).count(),
            1,
            "un seul PUSI"
        );
        assert!(
            after
                .iter()
                .map(|p| p.0)
                .eq(before.iter().take(after.len()).map(|p| p.0))
        );
        for (i, p) in ts.chunks(188).enumerate() {
            if pid_of(p) == 0x300 {
                assert_eq!(&out[i * 188..(i + 1) * 188], p);
            }
        }
        // Compteurs continus, PCR du 1er emplacement conservé (pas les autres : paquets sans PCR
        // d'origine ou devenus nuls).
        let ccs: Vec<u8> = after.iter().map(|p| p.1[3] & 0x0F).collect();
        assert!(ccs.iter().enumerate().all(|(k, &c)| usize::from(c) == k));
        let first = parse_packet(&after[0].1).unwrap();
        assert_eq!(
            first.adaptation.unwrap().pcr,
            Some(Pcr { base: 10, ext: 1 })
        );
        assert!(first.adaptation.unwrap().random_access);
        let second_pcr = after
            .iter()
            .skip(1)
            .filter_map(|p| parse_packet(&p.1).unwrap().adaptation.and_then(|a| a.pcr))
            .count();
        assert_eq!(
            second_pcr, 1,
            "le PCR du 2e PES d'origine tombe sur un emplacement conservé"
        );
    }

    #[test]
    fn rewrite_video_pes_inserts_missing_packets_after_the_last_video_packet() {
        let ts = video_stream();
        let out = rewrite_video_pes(&ts, 0x100, |orig| {
            // Chaque PES est écrit deux fois : plus de paquets que d'emplacements.
            orig.iter().chain(orig.iter()).cloned().collect()
        });
        let before = video_of(&ts);
        let after = video_of(&out);
        assert!(after.len() > before.len());
        assert_eq!(out.len(), ts.len() + (after.len() - before.len()) * 188);
        // Les paquets en plus suivent le dernier paquet vidéo d'origine.
        let last_original = before.last().unwrap().0;
        let tail: Vec<usize> = after.iter().skip(before.len()).map(|p| p.0).collect();
        assert!(
            tail.iter()
                .enumerate()
                .all(|(k, &i)| i == last_original + 1 + k)
        );
        assert_eq!(after.iter().filter(|p| p.1[1] & 0x40 != 0).count(), 6);
    }

    #[test]
    fn pes_helpers_round_trip_through_parse() {
        let pes = pes_bytes(Some(0x1_FFFF_FFFF), Some(5), &[1, 2, 3]);
        let h = crate::pes::parse_pes_header(&pes).unwrap();
        assert_eq!(
            (h.pts, h.dts, h.header_len),
            (Some(0x1_FFFF_FFFF), Some(5), 19)
        );
        assert_eq!(usize::from(h.packet_length), pes.len() - 6);
        // Trop grand pour 16 bits : longueur 0.
        let big = pes_bytes(None, None, &vec![0; 70_000]);
        assert_eq!(&big[4..6], &[0, 0]);
    }

    #[test]
    fn fill_nulls_replaces_the_next_null_in_order() {
        let ts = small_stream(3);
        // Deux cibles sur le même emplacement : la 2e prend le nul suivant.
        let out = fill_nulls(&ts, &[0, 2, 2], |at, rank| {
            packet(0x200, true, rank as u8, None, &[at as u8])
        })
        .unwrap();
        assert_eq!(
            pids(&out),
            vec![
                0x1000, 0x100, 0x200, 0x200, 0x200, 0x100, 0x1000, 0x100, 0x1FFF, 0x1FFF, 0x1FFF
            ]
        );
        assert_eq!(out[2 * 188 + 4], 2);
        assert_eq!(out[4 * 188 + 3] & 0x0F, 2);
        // Plus assez de nuls.
        assert!(fill_nulls(&ts, &[0; 7], |_, _| [0x47; 188]).is_err());
    }
}
