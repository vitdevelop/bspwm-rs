//! Reading a monitor's make and model from its EDID, for the output's
//! `wl_output`/`xdg_output` description (`bspc`-less tools like
//! `wlr-randr` and bars show it).
//!
//! `smithay-drm-extras`' own EDID support needs the system
//! `libdisplay-info`, whose installed version it rejects (see
//! `Cargo.toml`), so this parses just the two fields needed by hand.

/// The `(make, model)` an EDID block names: `make` is the three-letter PnP
/// manufacturer id (e.g. `DEL`), `model` the monitor name descriptor if
/// present, else the hex product code. `None` if `edid` is too short or
/// lacks the EDID header.
pub fn make_and_model(edid: &[u8]) -> Option<(String, String)> {
    const HEADER: [u8; 8] = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];
    if edid.len() < 128 || edid[..8] != HEADER {
        return None;
    }
    // Bytes 8-9, big endian: three 5-bit letters, 'A' = 1.
    let id = u16::from_be_bytes([edid[8], edid[9]]);
    let letter = |shift: u16| {
        let n = ((id >> shift) & 0x1F) as u8;
        (b'A' + n.saturating_sub(1)) as char
    };
    let make: String = [letter(10), letter(5), letter(0)].iter().collect();

    // Four 18-byte descriptors from byte 54; a "monitor name" one has a
    // zero 3-byte prefix and tag 0xFC, then 13 bytes of text ending in '\n'.
    let name = (0..4).find_map(|i| {
        let d = &edid[54 + i * 18..54 + (i + 1) * 18];
        if d[..3] == [0, 0, 0] && d[3] == 0xFC {
            let text: String = d[5..18]
                .iter()
                .take_while(|&&b| b != 0x0A)
                .map(|&b| b as char)
                .collect();
            let text = text.trim().to_string();
            (!text.is_empty()).then_some(text)
        } else {
            None
        }
    });
    let model = name.unwrap_or_else(|| format!("{:04X}", u16::from_le_bytes([edid[10], edid[11]])));
    Some((make, model))
}

#[cfg(test)]
mod tests {
    use super::make_and_model;

    fn edid_with(id: [u8; 2], product: [u8; 2], name: Option<&str>) -> Vec<u8> {
        let mut e = vec![0u8; 128];
        e[..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
        e[8..10].copy_from_slice(&id);
        e[10..12].copy_from_slice(&product);
        if let Some(name) = name {
            let d = &mut e[72..90];
            d[3] = 0xFC;
            let bytes = name.as_bytes();
            d[5..5 + bytes.len()].copy_from_slice(bytes);
            d[5 + bytes.len()] = 0x0A;
        }
        e
    }

    #[test]
    fn reads_the_pnp_id_and_the_monitor_name() {
        // 'D','E','L' = 4, 5, 12 -> 00100 00101 01100 -> 0x10AC
        let edid = edid_with([0x10, 0xAC], [0x34, 0x12], Some("DELL U2720Q"));
        assert_eq!(make_and_model(&edid), Some(("DEL".into(), "DELL U2720Q".into())));
    }

    #[test]
    fn falls_back_to_the_product_code_without_a_name() {
        let edid = edid_with([0x10, 0xAC], [0x34, 0x12], None);
        assert_eq!(make_and_model(&edid), Some(("DEL".into(), "1234".into())));
    }

    #[test]
    fn rejects_short_or_headerless_data() {
        assert_eq!(make_and_model(&[0u8; 10]), None);
        assert_eq!(make_and_model(&[0u8; 128]), None);
    }
}
