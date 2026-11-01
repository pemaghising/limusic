//! Rewrites YouTube's fragmented (DASH) AAC into an ordinary M4A. googlevideo serves itag 140 as
//! `moov` + `sidx` + a run of `moof`/`mdat` fragments; mpv and VLC play that, but Apple Music and
//! QuickTime refuse it. Only the container changes: the AAC frames are copied byte for byte into
//! one `mdat` behind a regular sample table, with `moov` first so the file opens without seeking.
use std::ops::Range;

struct Atom {
    kind: [u8; 4],
    start: usize,
    body: Range<usize>,
    end: usize,
}

const BAD: &str = "Unexpected MP4 layout.";

fn read<const N: usize>(data: &[u8], at: usize) -> Result<[u8; N], String> {
    data.get(at..at + N).and_then(|s| s.try_into().ok()).ok_or_else(|| BAD.to_owned())
}
fn be32(data: &[u8], at: usize) -> Result<u32, String> {
    read::<4>(data, at).map(u32::from_be_bytes)
}
fn be64(data: &[u8], at: usize) -> Result<u64, String> {
    read::<8>(data, at).map(u64::from_be_bytes)
}

fn atoms(data: &[u8], range: Range<usize>) -> Result<Vec<Atom>, String> {
    let mut out = Vec::new();
    let mut at = range.start;
    while at < range.end {
        let size = be32(data, at)? as u64;
        let kind = read::<4>(data, at + 4)?;
        let (header, size) = match size {
            1 => (16, be64(data, at + 8)?),
            0 => (8, (range.end - at) as u64),
            n => (8, n),
        };
        if size < header as u64 || size > (range.end - at) as u64 {
            return Err(BAD.into());
        }
        let end = at + size as usize;
        out.push(Atom { kind, start: at, body: at + header..end, end });
        at = end;
    }
    Ok(out)
}

fn find<'a>(list: &'a [Atom], kind: &[u8; 4]) -> Option<&'a Atom> {
    list.iter().find(|a| &a.kind == kind)
}
fn child(data: &[u8], parent: &Atom, kind: &[u8; 4]) -> Result<Atom, String> {
    atoms(data, parent.body.clone())?
        .into_iter()
        .find(|a| &a.kind == kind)
        .ok_or_else(|| BAD.to_owned())
}

fn atom(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 8);
    out.extend_from_slice(&(body.len() as u32 + 8).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    out
}
fn full(kind: &[u8; 4], words: &[u32]) -> Vec<u8> {
    let mut body = vec![0u8; 4]; // version 0, no flags
    for w in words {
        body.extend_from_slice(&w.to_be_bytes());
    }
    atom(kind, &body)
}

/// Whether this is a fragmented MP4 that [`defragment`] should rewrite.
pub fn is_fragmented(data: &[u8]) -> bool {
    atoms(data, 0..data.len()).is_ok_and(|top| find(&top, b"moof").is_some())
}

struct Tables {
    /// (size, duration) per sample, in media timescale.
    samples: Vec<(u32, u32)>,
    /// Samples per chunk; one chunk per fragment run.
    chunks: Vec<u32>,
    payload: Vec<u8>,
}

fn collect(data: &[u8], top: &[Atom], track_id: u32, trex: &Atom) -> Result<Tables, String> {
    // trex: version/flags, track_ID, default description index, duration, size, flags.
    let b = trex.body.start;
    let (default_duration, default_size) = (be32(data, b + 12)?, be32(data, b + 16)?);
    let mut tables = Tables { samples: Vec::new(), chunks: Vec::new(), payload: Vec::new() };
    for moof in top.iter().filter(|a| &a.kind == b"moof") {
        for traf in atoms(data, moof.body.clone())?.iter().filter(|a| &a.kind == b"traf") {
            let tfhd = child(data, traf, b"tfhd")?;
            let flags = be32(data, tfhd.body.start)? & 0xff_ffff;
            if be32(data, tfhd.body.start + 4)? != track_id {
                return Err("The audio has more than one track.".into());
            }
            let mut at = tfhd.body.start + 8;
            let mut base = moof.start as u64;
            if flags & 0x01 != 0 {
                base = be64(data, at)?;
                at += 8;
            }
            if flags & 0x02 != 0 {
                if be32(data, at)? != 1 {
                    return Err(BAD.into());
                }
                at += 4;
            }
            let mut duration = default_duration;
            let mut size = default_size;
            if flags & 0x08 != 0 {
                duration = be32(data, at)?;
                at += 4;
            }
            if flags & 0x10 != 0 {
                size = be32(data, at)?;
            }
            let mut cursor = base;
            for trun in atoms(data, traf.body.clone())?.iter().filter(|a| &a.kind == b"trun") {
                let flags = be32(data, trun.body.start)? & 0xff_ffff;
                let count = be32(data, trun.body.start + 4)?;
                let mut at = trun.body.start + 8;
                if flags & 0x01 != 0 {
                    let offset = be32(data, at)? as i32 as i64;
                    cursor = u64::try_from(base as i64 + offset).map_err(|_| BAD)?;
                    at += 4;
                }
                if flags & 0x04 != 0 {
                    at += 4;
                }
                let per_sample = [0x100, 0x200, 0x400, 0x800].iter().filter(|f| flags & *f != 0);
                if (count as usize).saturating_mul(per_sample.count() * 4) > trun.end - at {
                    return Err(BAD.into());
                }
                for _ in 0..count {
                    let (mut d, mut s) = (duration, size);
                    if flags & 0x100 != 0 {
                        d = be32(data, at)?;
                        at += 4;
                    }
                    if flags & 0x200 != 0 {
                        s = be32(data, at)?;
                        at += 4;
                    }
                    at += 4 * ((flags & 0x400 != 0) as usize + (flags & 0x800 != 0) as usize);
                    let start = usize::try_from(cursor).map_err(|_| BAD)?;
                    let bytes = data.get(start..start + s as usize).ok_or("Truncated audio.")?;
                    tables.payload.extend_from_slice(bytes);
                    tables.samples.push((s, d));
                    cursor += s as u64;
                }
                if count > 0 {
                    tables.chunks.push(count);
                }
            }
        }
    }
    if tables.samples.is_empty() {
        return Err("The audio has no samples.".into());
    }
    Ok(tables)
}

fn sample_tables(t: &Tables, mdat_data: u32) -> Vec<u8> {
    let mut stts: Vec<(u32, u32)> = Vec::new();
    for &(_, d) in &t.samples {
        match stts.last_mut() {
            Some((n, delta)) if *delta == d => *n += 1,
            _ => stts.push((1, d)),
        }
    }
    let mut words = vec![stts.len() as u32];
    words.extend(stts.iter().flat_map(|&(n, d)| [n, d]));
    let mut out = full(b"stts", &words);

    let mut stsc: Vec<(u32, u32)> = Vec::new();
    for (i, &n) in t.chunks.iter().enumerate() {
        if stsc.last().is_none_or(|&(_, last)| last != n) {
            stsc.push((i as u32 + 1, n));
        }
    }
    let mut words = vec![stsc.len() as u32];
    words.extend(stsc.iter().flat_map(|&(first, n)| [first, n, 1]));
    out.extend(full(b"stsc", &words));

    let first = t.samples[0].0;
    let words = if t.samples.iter().all(|&(s, _)| s == first) {
        vec![first, t.samples.len() as u32]
    } else {
        let mut w = vec![0, t.samples.len() as u32];
        w.extend(t.samples.iter().map(|&(s, _)| s));
        w
    };
    out.extend(full(b"stsz", &words));

    let mut words = vec![t.chunks.len() as u32];
    let mut offset = mdat_data;
    let mut sample = 0usize;
    for &n in &t.chunks {
        words.push(offset);
        for &(s, _) in &t.samples[sample..sample + n as usize] {
            offset += s;
        }
        sample += n as usize;
    }
    out.extend(full(b"stco", &words));
    out
}

/// Copy a header atom with its duration field replaced (mvhd/mdhd/tkhd, version 0 or 1).
fn with_duration(data: &[u8], a: &Atom, duration: u64) -> Result<Vec<u8>, String> {
    let mut out = data[a.start..a.end].to_vec();
    let body = a.body.start - a.start;
    let v1 = out.get(body) == Some(&1);
    let at = body
        + match (&a.kind, v1) {
            (b"tkhd", false) => 20,
            (b"tkhd", true) => 28,
            (_, false) => 16,
            (_, true) => 24,
        };
    if v1 {
        out.get_mut(at..at + 8).ok_or(BAD)?.copy_from_slice(&duration.to_be_bytes());
    } else {
        let d = u32::try_from(duration).map_err(|_| BAD)?;
        out.get_mut(at..at + 4).ok_or(BAD)?.copy_from_slice(&d.to_be_bytes());
    }
    Ok(out)
}

fn timescale(data: &[u8], a: &Atom) -> Result<u32, String> {
    let v1 = data.get(a.body.start) == Some(&1);
    be32(data, a.body.start + if v1 { 20 } else { 12 })
}

/// The edit list with an open-ended segment (duration 0, as DASH writes it) closed at the end of
/// the media, and the presented duration it adds up to. The edit is what skips the encoder's
/// priming samples, so keeping it keeps playback sample-exact.
fn closed_edits(
    data: &[u8],
    edts: &Atom,
    media: u64,
    scales: (u32, u32),
) -> Result<(Vec<u8>, u64), String> {
    let elst = child(data, edts, b"elst")?;
    let mut bytes = data[elst.start..elst.end].to_vec();
    let body = elst.body.start - elst.start;
    let v1 = bytes.get(body) == Some(&1);
    let count = be32(&bytes, body + 4)? as usize;
    let (width, entry) = if v1 { (8, 20) } else { (4, 12) };
    let mut presented = 0u64;
    for i in 0..count {
        let at = body + 8 + i * entry;
        let read = |at| if v1 { be64(&bytes, at) } else { be32(&bytes, at).map(u64::from) };
        let mut duration = read(at)?;
        let start = read(at + width)?;
        let start = if v1 { start as i64 } else { start as u32 as i32 as i64 };
        if duration == 0 && count == 1 && start >= 0 {
            let rest = media.saturating_sub(start as u64);
            duration = (rest as u128 * scales.0 as u128 / scales.1 as u128) as u64;
            let field = bytes.get_mut(at..at + width).ok_or(BAD)?;
            if v1 {
                field.copy_from_slice(&duration.to_be_bytes());
            } else {
                field.copy_from_slice(&u32::try_from(duration).map_err(|_| BAD)?.to_be_bytes());
            }
        }
        presented += duration;
    }
    Ok((atom(b"edts", &bytes), presented))
}

/// Rebuild `parent` from its children, letting `edit` replace (Some) or drop (None) each one.
fn rebuild(
    data: &[u8],
    parent: &Atom,
    edit: &mut dyn FnMut(&Atom) -> Result<Option<Vec<u8>>, String>,
) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    for c in atoms(data, parent.body.clone())? {
        if let Some(bytes) = edit(&c)? {
            body.extend(bytes);
        }
    }
    Ok(atom(&parent.kind, &body))
}

/// The fragmented file as a regular M4A, or an error when the layout is not the single-track
/// audio this was written for (the caller then keeps the original, which still plays in-app).
pub fn defragment(data: &[u8]) -> Result<Vec<u8>, String> {
    let top = atoms(data, 0..data.len())?;
    let moov = find(&top, b"moov").ok_or(BAD)?;
    let children = atoms(data, moov.body.clone())?;
    let traks: Vec<&Atom> = children.iter().filter(|a| &a.kind == b"trak").collect();
    let [trak] = traks[..] else { return Err("The audio has more than one track.".into()) };
    let mvhd = find(&children, b"mvhd").ok_or(BAD)?;
    let tkhd = child(data, trak, b"tkhd")?;
    let track_id =
        be32(data, tkhd.body.start + if data.get(tkhd.body.start) == Some(&1) { 20 } else { 12 })?;
    let trex = child(data, find(&children, b"mvex").ok_or(BAD)?, b"trex")?;
    let mdia = child(data, trak, b"mdia")?;
    let mdhd = child(data, &mdia, b"mdhd")?;
    let tables = collect(data, &top, track_id, &trex)?;

    let media = tables.samples.iter().map(|&(_, d)| d as u64).sum::<u64>();
    let (movie_scale, media_scale) = (timescale(data, mvhd)?, timescale(data, &mdhd)?);
    if media_scale == 0 {
        return Err(BAD.into());
    }
    let (edits, movie) = match child(data, trak, b"edts") {
        Ok(edts) => {
            let (bytes, presented) = closed_edits(data, &edts, media, (movie_scale, media_scale))?;
            (Some(bytes), presented)
        }
        Err(_) => (None, (media as u128 * movie_scale as u128 / media_scale as u128) as u64),
    };

    let ftyp = atom(b"ftyp", b"M4A \0\0\x02\0M4A mp42isom");
    let build = |mdat_data: u32| -> Result<Vec<u8>, String> {
        let stbl = |a: &Atom| -> Result<Option<Vec<u8>>, String> {
            // Only the codec description survives; every other table described the fragments.
            let stsd = child(data, a, b"stsd")?;
            let mut body = data[stsd.start..stsd.end].to_vec();
            body.extend(sample_tables(&tables, mdat_data));
            Ok(Some(atom(b"stbl", &body)))
        };
        let minf = |a: &Atom| {
            rebuild(data, a, &mut |c| match &c.kind {
                b"stbl" => stbl(c),
                _ => Ok(Some(data[c.start..c.end].to_vec())),
            })
        };
        let mdia = |a: &Atom| {
            rebuild(data, a, &mut |c| match &c.kind {
                b"mdhd" => with_duration(data, c, media).map(Some),
                b"minf" => minf(c).map(Some),
                _ => Ok(Some(data[c.start..c.end].to_vec())),
            })
        };
        rebuild(data, moov, &mut |c| match &c.kind {
            b"mvhd" => with_duration(data, c, movie).map(Some),
            // Fragment defaults describe the old layout.
            b"mvex" => Ok(None),
            b"trak" => rebuild(data, c, &mut |t| match &t.kind {
                b"tkhd" => with_duration(data, t, movie).map(Some),
                b"edts" => Ok(edits.clone()),
                b"mdia" => mdia(t).map(Some),
                _ => Ok(Some(data[t.start..t.end].to_vec())),
            })
            .map(Some),
            _ => Ok(Some(data[c.start..c.end].to_vec())),
        })
    };
    // The sample table's size does not depend on the offsets, so one dry run fixes them.
    let moov_len = build(0)?.len();
    let mdat_data = u32::try_from(ftyp.len() + moov_len + 8).map_err(|_| BAD)?;
    let mdat_len = u32::try_from(tables.payload.len() + 8).map_err(|_| BAD)?;
    let mut out = ftyp;
    out.extend(build(mdat_data)?);
    out.extend_from_slice(&mdat_len.to_be_bytes());
    out.extend_from_slice(b"mdat");
    out.extend_from_slice(&tables.payload);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(kind: &[u8; 4], words: &[u32]) -> Vec<u8> {
        full(kind, words)
    }

    /// A two-fragment, single-track file shaped like googlevideo's itag 140: defaults in `trex`,
    /// per-sample sizes in `trun`, `default-base-is-moof`, and a `sidx` in front.
    fn fragmented() -> (Vec<u8>, Vec<u8>) {
        let mvhd = header(
            b"mvhd",
            &[
                0, 0, 1000, 0, 0x10000, 0x01000000, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                2,
            ],
        );
        let tkhd =
            header(b"tkhd", &[0, 0, 1, 0, 0, 0, 0, 0, 0x01000000, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        let mdhd = header(b"mdhd", &[0, 0, 44100, 0, 0]);
        let stsd = header(b"stsd", &[0]);
        let empty = [
            header(b"stts", &[0]),
            header(b"stsc", &[0]),
            header(b"stsz", &[0, 0]),
            header(b"stco", &[0]),
        ]
        .concat();
        let stbl = atom(b"stbl", &[stsd, empty].concat());
        let minf = atom(b"minf", &stbl);
        let mdia = atom(b"mdia", &[mdhd, minf].concat());
        // One open-ended edit skipping 1024 priming samples, as googlevideo writes it.
        let edts = atom(b"edts", &header(b"elst", &[1, 0, 1024, 0x10000]));
        let trak = atom(b"trak", &[tkhd, edts, mdia].concat());
        let trex = header(b"trex", &[1, 1, 1024, 0, 0]);
        let moov = atom(b"moov", &[mvhd, trak, atom(b"mvex", &trex)].concat());
        let mut file =
            [atom(b"ftyp", b"dash\0\0\0\0iso6mp41"), moov, header(b"sidx", &[1, 44100, 0, 0, 0])]
                .concat();
        let mut audio = Vec::new();
        for (fragment, sizes) in [[3u32, 5], [4, 2]].iter().enumerate() {
            let samples: Vec<u8> = sizes
                .iter()
                .enumerate()
                .flat_map(|(i, &s)| vec![(fragment * 16 + i) as u8; s as usize])
                .collect();
            audio.extend(&samples);
            let tfhd = {
                let mut b = vec![0, 0x02, 0, 0]; // default-base-is-moof
                b.extend(1u32.to_be_bytes());
                atom(b"tfhd", &b)
            };
            let tfdt = header(b"tfdt", &[fragment as u32 * 2048]);
            let trun_len = 8 + 4 + 4 + 4 + 4 * sizes.len();
            let moof_len = 8 + 16 + 8 + tfhd.len() + tfdt.len() + trun_len;
            let mut trun = vec![0, 0, 0x02, 0x01]; // data offset, per-sample size
            trun.extend((sizes.len() as u32).to_be_bytes());
            trun.extend((moof_len as u32 + 8).to_be_bytes());
            for s in sizes {
                trun.extend(s.to_be_bytes());
            }
            let traf = atom(b"traf", &[tfhd, tfdt, atom(b"trun", &trun)].concat());
            let moof = atom(b"moof", &[header(b"mfhd", &[fragment as u32 + 1]), traf].concat());
            assert_eq!(moof.len(), moof_len);
            file.extend(moof);
            file.extend(atom(b"mdat", &samples));
        }
        (file, audio)
    }

    #[test]
    fn fragments_become_one_indexed_mdat_with_identical_audio() {
        let (file, audio) = fragmented();
        assert!(is_fragmented(&file));
        let out = defragment(&file).unwrap();
        assert!(!is_fragmented(&out));
        let top = atoms(&out, 0..out.len()).unwrap();
        let kinds: Vec<&[u8; 4]> = top.iter().map(|a| &a.kind).collect();
        assert_eq!(kinds, [b"ftyp", b"moov", b"mdat"]);
        assert_eq!(&out[top[2].body.clone()], &audio[..]);

        let moov = &top[1];
        let inner = atoms(&out, moov.body.clone()).unwrap();
        assert!(find(&inner, b"mvex").is_none());
        let mvhd = find(&inner, b"mvhd").unwrap();
        assert_eq!(be32(&out, mvhd.body.start + 16).unwrap(), (4096 - 1024) * 1000 / 44100);
        let trak = find(&inner, b"trak").unwrap();
        let elst = child(&out, &child(&out, trak, b"edts").unwrap(), b"elst").unwrap();
        let edit: Vec<u32> =
            (elst.body.start + 4..elst.end).step_by(4).map(|i| be32(&out, i).unwrap()).collect();
        assert_eq!(edit, [1, (4096 - 1024) * 1000 / 44100, 1024, 0x10000]);
        let mdia = child(&out, trak, b"mdia").unwrap();
        assert_eq!(be32(&out, child(&out, &mdia, b"mdhd").unwrap().body.start + 16).unwrap(), 4096);
        let stbl = child(&out, &child(&out, &mdia, b"minf").unwrap(), b"stbl").unwrap();
        let words = |kind: &[u8; 4]| -> Vec<u32> {
            let a = child(&out, &stbl, kind).unwrap();
            (a.body.start + 4..a.end).step_by(4).map(|i| be32(&out, i).unwrap()).collect()
        };
        assert_eq!(words(b"stts"), [1, 4, 1024]);
        assert_eq!(words(b"stsc"), [1, 1, 2, 1]);
        assert_eq!(words(b"stsz"), [0, 4, 3, 5, 4, 2]);
        let data = top[2].body.start as u32;
        assert_eq!(words(b"stco"), [2, data, data + 8]);
    }

    #[test]
    fn truncated_or_progressive_files_are_left_alone() {
        let (file, _) = fragmented();
        assert!(defragment(&file[..file.len() - 3]).is_err());
        let progressive = defragment(&file).unwrap();
        assert!(!is_fragmented(&progressive));
        assert!(defragment(&progressive).is_err());
    }
}
