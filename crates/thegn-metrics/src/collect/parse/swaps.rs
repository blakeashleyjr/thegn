//! Header-driven parser for `/proc/swaps`.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwapKind {
    Partition,
    File,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SwapDev {
    pub name: String,
    pub kind: SwapKind,
    pub size_bytes: u64,
    pub used_bytes: u64,
    pub priority: i32,
}

pub fn parse(buf: &[u8], out: &mut Vec<SwapDev>) {
    let mut lines = buf.split(|byte| *byte == b'\n');
    let Some(header) = lines.next() else {
        out.clear();
        return;
    };
    let mut indices = [None; 5];
    for (index, column) in header
        .split(u8::is_ascii_whitespace)
        .filter(|col| !col.is_empty())
        .enumerate()
    {
        let slot = match column {
            b"Filename" => Some(0),
            b"Type" => Some(1),
            b"Size" => Some(2),
            b"Used" => Some(3),
            b"Priority" => Some(4),
            _ => None,
        };
        if let Some(slot) = slot {
            indices[slot] = Some(index);
        }
    }
    let [
        Some(name_i),
        Some(type_i),
        Some(size_i),
        Some(used_i),
        Some(priority_i),
    ] = indices
    else {
        out.clear();
        return;
    };
    let mut count = 0;
    for line in lines {
        if count == 32 {
            break;
        }
        let mut row = [None; 5];
        for (index, value) in line
            .split(u8::is_ascii_whitespace)
            .filter(|col| !col.is_empty())
            .enumerate()
        {
            for (slot, column) in [name_i, type_i, size_i, used_i, priority_i]
                .into_iter()
                .enumerate()
            {
                if index == column {
                    row[slot] = Some(value);
                }
            }
        }
        let name = row[0];
        let kind = row[1];
        let size = row[2].and_then(parse_u64);
        let used = row[3].and_then(parse_u64);
        let priority = row[4].and_then(parse_i32);
        let (Some(name), Some(kind), Some(size), Some(used), Some(priority)) =
            (name, kind, size, used, priority)
        else {
            continue;
        };
        let (Some(size_bytes), Some(used_bytes)) = (size.checked_mul(1024), used.checked_mul(1024))
        else {
            continue;
        };
        let kind = if kind == b"partition" {
            SwapKind::Partition
        } else if kind == b"file" {
            SwapKind::File
        } else {
            SwapKind::Other
        };
        let name = std::str::from_utf8(name).unwrap_or("");
        if name.is_empty() {
            continue;
        }
        if count == out.len() {
            out.push(SwapDev {
                name: String::new(),
                kind,
                size_bytes,
                used_bytes,
                priority,
            });
        }
        let dev = &mut out[count];
        if dev.name != name {
            dev.name.clear();
            dev.name.push_str(name);
        }
        dev.kind = kind;
        dev.size_bytes = size_bytes;
        dev.used_bytes = used_bytes;
        dev.priority = priority;
        count += 1;
    }
    out.truncate(count);
}

fn parse_u64(input: &[u8]) -> Option<u64> {
    if input.is_empty() {
        return None;
    }
    input.iter().try_fold(0_u64, |value, digit| {
        if !digit.is_ascii_digit() {
            return None;
        }
        value.checked_mul(10)?.checked_add(u64::from(*digit - b'0'))
    })
}

fn parse_i32(input: &[u8]) -> Option<i32> {
    if let Some(rest) = input.strip_prefix(b"-") {
        let magnitude = parse_u64(rest)?;
        i32::try_from(magnitude).ok()?.checked_neg()
    } else {
        i32::try_from(parse_u64(input)?).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::{SwapDev, SwapKind, parse};

    #[test]
    fn parses_reordered_columns_and_reuses_names() {
        let mut rows: Vec<SwapDev> = Vec::new();
        parse(
            include_bytes!("../../../tests/fixtures/mem/swaps_zram_file"),
            &mut rows,
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].kind, SwapKind::Partition);
        assert_eq!(
            (rows[0].size_bytes, rows[0].used_bytes, rows[0].priority),
            (2_097_152, 1_048_576, -1)
        );
        let ptr = rows[0].name.as_ptr();
        parse(b"Priority Extra Used Filename Size Type\n-1 x 1 /dev/zram0 2 partition\n2 x 0 /swap 4 file\n", &mut rows);
        assert_eq!(rows[0].name.as_ptr(), ptr);
        parse(b"Filename Type Size Used Priority\n", &mut rows);
        assert!(rows.is_empty());
    }
}
