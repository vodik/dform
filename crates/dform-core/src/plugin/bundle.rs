//! A wasm provider is one file, one artifact to package, distribute and
//! version: its component carries what else is its as custom sections,
//! which wasmtime does not read:
//!
//! - [`SIGNER`] (`dform.signer`): its signer's whole component (wit/host's
//!   `world signer-component`), which the host instantiates apart from it;
//! - [`SOURCES`] (`dform.sources`): its `.df` files (its `schema.df`, its
//!   docs), by path relative to its crate: each a little-endian `u32`
//!   length and the path's UTF-8, then a `u32` length and the file's
//!   bytes, one after another ([`Sources`]).
//!
//! `cargo xtask components` writes them ([`with_sections`]) and the host
//! reads them ([`section`]). Only the component's top level is walked: a
//! section is an id byte, its size (unsigned LEB128) and its contents; a
//! custom section's (id 0) are its name (a size and UTF-8) and its bytes.

/// Its signer's component.
pub const SIGNER: &str = "dform.signer";
/// Its `.df` sources.
pub const SOURCES: &str = "dform.sources";

/// The magic every wasm binary starts with, before its version and layer.
const MAGIC: &[u8; 4] = b"\0asm";

/// A custom section's id.
const CUSTOM: u8 = 0;

/// An unsigned LEB128 `u32` at `at`, and where it ends.
fn leb(bytes: &[u8], mut at: usize) -> Option<(u32, usize)> {
    let mut v: u64 = 0;
    for shift in (0..35).step_by(7) {
        let b = *bytes.get(at)?;
        at += 1;
        v |= u64::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return u32::try_from(v).ok().map(|v| (v, at));
        }
    }
    None
}

fn push_leb(out: &mut Vec<u8>, mut v: u32) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        match v {
            0 => return out.push(b),
            _ => out.push(b | 0x80),
        }
    }
}

/// A top-level section: its id, and the range of the whole section and
/// of its contents.
struct Section {
    id: u8,
    whole: std::ops::Range<usize>,
    contents: std::ops::Range<usize>,
}

/// Each top-level section of the wasm binary `wasm`.
fn walk(wasm: &[u8]) -> Result<Vec<Section>, String> {
    if wasm.len() < 8 || &wasm[..4] != MAGIC {
        return Err("not a wasm binary".into());
    }
    let mut out = Vec::new();
    let mut at = 8;
    while at < wasm.len() {
        let id = wasm[at];
        let (size, start) = leb(wasm, at + 1).ok_or("a section's size is cut short")?;
        let end = start
            .checked_add(size as usize)
            .filter(|e| *e <= wasm.len())
            .ok_or("a section runs past the end")?;
        out.push(Section {
            id,
            whole: at..end,
            contents: start..end,
        });
        at = end;
    }
    Ok(out)
}

/// A custom section's name and bytes.
fn custom<'a>(wasm: &'a [u8], s: &Section) -> Option<(&'a str, &'a [u8])> {
    if s.id != CUSTOM {
        return None;
    }
    let c = &wasm[s.contents.clone()];
    let (len, start) = leb(c, 0)?;
    let end = start.checked_add(len as usize).filter(|e| *e <= c.len())?;
    Some((std::str::from_utf8(&c[start..end]).ok()?, &c[end..]))
}

/// The bytes of the top-level custom section `name` of `wasm`, if it has
/// one (the first, if more).
pub fn section<'a>(wasm: &'a [u8], name: &str) -> Result<Option<&'a [u8]>, String> {
    Ok(walk(wasm)?
        .iter()
        .filter_map(|s| custom(wasm, s))
        .find(|(n, _)| *n == name)
        .map(|(_, bytes)| bytes))
}

/// `wasm` with the custom sections `add`, in place of any of their names
/// it had: so writing them again changes nothing.
pub fn with_sections(wasm: &[u8], add: &[(&str, &[u8])]) -> Result<Vec<u8>, String> {
    let mut out = wasm[..8.min(wasm.len())].to_vec();
    for s in walk(wasm)? {
        let replaced = custom(wasm, &s).is_some_and(|(n, _)| add.iter().any(|(a, _)| *a == n));
        if !replaced {
            out.extend_from_slice(&wasm[s.whole.clone()]);
        }
    }
    for (name, bytes) in add {
        let mut contents = Vec::new();
        push_leb(
            &mut contents,
            u32::try_from(name.len()).map_err(|_| "a long name")?,
        );
        contents.extend_from_slice(name.as_bytes());
        contents.extend_from_slice(bytes);
        out.push(CUSTOM);
        push_leb(
            &mut out,
            u32::try_from(contents.len()).map_err(|_| "a section past 4 GiB")?,
        );
        out.extend(contents);
    }
    Ok(out)
}

/// A provider's `.df` sources ([`SOURCES`]), by path relative to its
/// crate.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Sources(pub Vec<(String, Vec<u8>)>);

impl Sources {
    /// The section's bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for (path, bytes) in &self.0 {
            for part in [path.as_bytes(), bytes] {
                out.extend((part.len() as u32).to_le_bytes());
                out.extend_from_slice(part);
            }
        }
        out
    }

    /// The files of a section's bytes; refused when a path is not a
    /// plain relative one (it is written under the host's cache).
    pub fn decode(mut bytes: &[u8]) -> Result<Sources, String> {
        let mut files = Vec::new();
        while !bytes.is_empty() {
            let path = take(&mut bytes)?;
            let path = std::str::from_utf8(path).map_err(|_| "a path that is not UTF-8")?;
            let plain = !path.is_empty()
                && !path.starts_with('/')
                && path
                    .split('/')
                    .all(|p| !p.is_empty() && p != "." && p != "..");
            if !plain {
                return Err(format!("the path {path:?} is not a plain relative one"));
            }
            files.push((path.to_string(), take(&mut bytes)?.to_vec()));
        }
        Ok(Sources(files))
    }
}

/// The next length-prefixed part of `bytes`, taken off its front.
fn take<'a>(bytes: &mut &'a [u8]) -> Result<&'a [u8], String> {
    let (len, rest) = bytes.split_first_chunk::<4>().ok_or("cut short")?;
    let len = u32::from_le_bytes(*len) as usize;
    let (part, rest) = rest.split_at_checked(len).ok_or("cut short")?;
    *bytes = rest;
    Ok(part)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The smallest component: its magic, version 13, layer 1.
    const EMPTY: &[u8] = b"\0asm\x0d\x00\x01\x00";

    /// Sections written are read back; written again, they replace the
    /// ones there; a section of another name is kept.
    #[test]
    fn sections_are_written_once() {
        let one = with_sections(EMPTY, &[("other", b"x"), (SIGNER, b"s1")]).unwrap();
        assert_eq!(section(&one, SIGNER).unwrap(), Some(&b"s1"[..]));
        let two = with_sections(&one, &[(SIGNER, b"s2"), (SOURCES, b"")]).unwrap();
        assert_eq!(section(&two, SIGNER).unwrap(), Some(&b"s2"[..]));
        assert_eq!(section(&two, SOURCES).unwrap(), Some(&b""[..]));
        assert_eq!(section(&two, "other").unwrap(), Some(&b"x"[..]));
        assert_eq!(
            with_sections(&two, &[(SIGNER, b"s2"), (SOURCES, b"")]).unwrap(),
            two
        );
        assert_eq!(section(EMPTY, SIGNER).unwrap(), None);
        assert!(section(b"not wasm", SIGNER).is_err());
    }

    /// Sources are their paths and bytes; a path out of its directory is
    /// refused.
    #[test]
    fn sources_round_trip() {
        let s = Sources(vec![
            ("src/schema.df".into(), b"type ovh.instance".to_vec()),
            ("doc/a.df".into(), Vec::new()),
        ]);
        assert_eq!(Sources::decode(&s.encode()).unwrap(), s);
        for bad in ["../x.df", "/etc/x.df", "a//b", ""] {
            let s = Sources(vec![(bad.into(), Vec::new())]);
            assert!(Sources::decode(&s.encode()).is_err(), "{bad}");
        }
        assert!(Sources::decode(b"\x05\x00\x00\x00ab").is_err());
    }
}
