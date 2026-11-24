// SPDX-License-Identifier: GPL-2.0-or-later

//! MP4 box and Matroska EBML primitives shared by the audio and video paths.

pub(crate) fn is_mp4_extension(extension: &str) -> bool {
    matches!(extension, "mp4" | "m4a" | "m4v" | "mov")
}

/// Reads an EBML variable-length integer, leaving `pos` after it. IDs keep
/// their length marker; sizes and values do not.
pub(crate) fn read_ebml_vint(
    input: &[u8],
    pos: &mut usize,
    max_length: usize,
    keep_marker: bool,
) -> Option<(u64, usize)> {
    let first = *input.get(*pos)?;
    let length = first.leading_zeros() as usize + 1;
    if length > max_length || *pos + length > input.len() {
        return None;
    }
    let mut value = if keep_marker {
        u64::from(first)
    } else {
        u64::from(first & ((0x80_u8 >> (length - 1)) - 1))
    };
    for byte in &input[*pos + 1..*pos + length] {
        value = (value << 8) | u64::from(*byte);
    }
    *pos += length;
    Some((value, length))
}

/// Reads an EBML element header. `None` in the size slot means the element has
/// an unknown size and extends to the end of its parent.
pub(crate) fn read_ebml_element(input: &[u8], pos: &mut usize) -> Option<(u32, Option<u64>)> {
    let (id, _) = read_ebml_vint(input, pos, 4, true)?;
    let (size, length) = read_ebml_vint(input, pos, 8, false)?;
    let unknown_size = size == (1_u64 << (7 * length)) - 1;
    Some((id as u32, if unknown_size { None } else { Some(size) }))
}

pub(crate) fn skip_ebml_element(input: &[u8], pos: &mut usize, size: Option<u64>) -> Option<()> {
    let end = (*pos as u64).checked_add(size?)?;
    if end > input.len() as u64 {
        return None;
    }
    *pos = end as usize;
    Some(())
}

pub(crate) fn ebml_element_end(start: usize, size: Option<u64>, limit: usize) -> usize {
    size.and_then(|size| (start as u64).checked_add(size))
        .map(|value| value.min(limit as u64) as usize)
        .unwrap_or(limit)
}

pub(crate) fn read_ebml_uint(input: &[u8], pos: usize, size: u64) -> Option<u64> {
    if size > 8 {
        return None;
    }
    let bytes = input.get(pos..pos + size as usize)?;
    let mut value = 0_u64;
    for byte in bytes {
        value = value.checked_shl(8)? | u64::from(*byte);
    }
    Some(value)
}

pub(crate) fn mp4_child_boxes(data: &[u8]) -> Vec<([u8; 4], &[u8])> {
    let mut boxes = Vec::new();
    let mut position = 0;
    while position + 8 <= data.len() {
        let size = u32::from_be_bytes([
            data[position],
            data[position + 1],
            data[position + 2],
            data[position + 3],
        ]) as usize;
        let kind: [u8; 4] = data[position + 4..position + 8]
            .try_into()
            .expect("the box type is four bytes");
        let (header, size) = if size == 1 {
            if position + 16 > data.len() {
                break;
            }
            let large = u64::from_be_bytes(
                data[position + 8..position + 16]
                    .try_into()
                    .expect("a 64-bit box size is eight bytes"),
            ) as usize;
            (16, large)
        } else if size == 0 {
            (8, data.len() - position)
        } else {
            (8, size)
        };
        if size < header || position + size > data.len() {
            break;
        }
        boxes.push((kind, &data[position + header..position + size]));
        position += size;
    }
    boxes
}

pub(crate) fn mp4_find<'a>(data: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    mp4_child_boxes(data)
        .into_iter()
        .find(|(box_kind, _)| box_kind == kind)
        .map(|(_, body)| body)
}
