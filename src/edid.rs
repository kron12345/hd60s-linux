//! EDID handling for the block the HD60 S presents to its HDMI source.
//!
//! The device stores it in an EEPROM reachable as bank 0xa0 (256 bytes, read
//! and written in 16-byte pieces). This module only validates and describes
//! EDID blocks; the USB access lives in the binary.

/// EDID the device reports in its power-on state (read from a Rev. 4 unit
/// before any host wrote to it): 1920x1080 preferred, monitor name "Elgato".
pub const FACTORY: [u8; 256] = [
    0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00, 0x14, 0xe1, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x11, 0x17, 0x01, 0x03, 0x80, 0x00, 0x02, 0x78, 0x1a, 0xcf, 0x74, 0xa3, 0x57, 0x4c, 0xb0, 0x23,
    0x09, 0x48, 0x4c, 0x00, 0x00, 0x00, 0x81, 0xc0, 0xd1, 0xc0, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01,
    0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x02, 0x3a, 0x80, 0x18, 0x71, 0x38, 0x2d, 0x40, 0x58, 0x2c,
    0x45, 0x00, 0xc4, 0x8e, 0x21, 0x00, 0x00, 0x1e, 0x00, 0x00, 0x00, 0x18, 0x00, 0x1c, 0x16, 0x20,
    0x58, 0x2c, 0x25, 0x00, 0xc4, 0x8e, 0x21, 0x00, 0x00, 0x9e, 0x00, 0x00, 0x00, 0xfc, 0x00, 0x45,
    0x6c, 0x67, 0x61, 0x74, 0x6f, 0x0a, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x00, 0x00, 0x00, 0xfd,
    0x00, 0x17, 0x3d, 0x19, 0x46, 0x0f, 0x00, 0x0a, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20, 0x01, 0xe5,
    0x02, 0x03, 0x26, 0xf4, 0x51, 0x85, 0x04, 0x03, 0x02, 0x12, 0x13, 0x94, 0x16, 0x07, 0x06, 0x11,
    0x15, 0xa1, 0xa2, 0x27, 0x1f, 0x10, 0x23, 0x09, 0x07, 0x01, 0x83, 0x01, 0x00, 0x00, 0x67, 0x03,
    0x0c, 0x00, 0x10, 0x00, 0x20, 0x28, 0x8c, 0x0a, 0xa0, 0x14, 0x51, 0xf0, 0x16, 0x00, 0x26, 0x7c,
    0x43, 0x00, 0xc4, 0x8e, 0x21, 0x00, 0x00, 0x98, 0x8c, 0x0a, 0xd0, 0x8a, 0x20, 0xe0, 0x2d, 0x10,
    0x10, 0x3e, 0x96, 0x00, 0xc4, 0x8e, 0x21, 0x00, 0x00, 0x19, 0x01, 0x1d, 0x00, 0x72, 0x51, 0xd0,
    0x1e, 0x20, 0x6e, 0x28, 0x55, 0x00, 0xc4, 0x8e, 0x21, 0x00, 0x00, 0x1f, 0x01, 0x1d, 0x80, 0x18,
    0x71, 0x1c, 0x16, 0x20, 0x58, 0x2c, 0x25, 0x00, 0xc4, 0x8e, 0x21, 0x00, 0x00, 0x9e, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x14,
];

const HEADER: [u8; 8] = [0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00];

/// Checks size, header and the checksum of both 128-byte blocks.
pub fn validate(edid: &[u8]) -> Result<(), String> {
    if edid.len() != 256 {
        return Err(format!("expected 256 bytes, got {}", edid.len()));
    }
    if edid[..8] != HEADER {
        return Err("missing EDID header".into());
    }
    for (index, block) in edid.chunks(128).enumerate() {
        let sum = block.iter().fold(0_u8, |acc, &b| acc.wrapping_add(b));
        if sum != 0 {
            return Err(format!(
                "block {index} checksum is off by {sum} (byte {} should be {:#04x})",
                index * 128 + 127,
                block[127].wrapping_sub(sum)
            ));
        }
    }
    Ok(())
}

/// Recomputes the checksum byte of both blocks.
pub fn fix_checksums(edid: &mut [u8]) {
    for block in edid.chunks_mut(128) {
        let sum = block[..127]
            .iter()
            .fold(0_u8, |acc, &b| acc.wrapping_add(b));
        block[127] = 0_u8.wrapping_sub(sum);
    }
}

/// Human-readable summary: manufacturer, monitor name, preferred timing.
pub fn summary(edid: &[u8]) -> String {
    if edid.len() < 128 {
        return "too short".into();
    }
    let id = u16::from_be_bytes([edid[8], edid[9]]);
    let letter = |shift: u16| (b'A' - 1 + ((id >> shift) & 0x1f) as u8) as char;
    let manufacturer: String = [letter(10), letter(5), letter(0)].iter().collect();
    let mut name = None;
    for at in (54..126).step_by(18) {
        if edid[at..at + 3] == [0, 0, 0] && edid[at + 3] == 0xfc {
            name = Some(
                String::from_utf8_lossy(&edid[at + 5..at + 18])
                    .trim_end_matches(['\n', ' ', '\0'])
                    .to_string(),
            );
        }
    }
    let d = &edid[54..72];
    let clock = u16::from_le_bytes([d[0], d[1]]) as u32 * 10;
    let h_active = d[2] as u32 | ((d[4] as u32 & 0xf0) << 4);
    let h_blank = d[3] as u32 | ((d[4] as u32 & 0x0f) << 8);
    let v_active = d[5] as u32 | ((d[7] as u32 & 0xf0) << 4);
    let v_blank = d[6] as u32 | ((d[7] as u32 & 0x0f) << 8);
    let refresh = if h_active + h_blank > 0 && v_active + v_blank > 0 {
        clock as f64 * 1000.0 / ((h_active + h_blank) as f64 * (v_active + v_blank) as f64)
    } else {
        0.0
    };
    format!(
        "manufacturer {manufacturer}, name {}, preferred {h_active}x{v_active} @ {refresh:.2} Hz ({} MHz), {} extension block(s)",
        name.as_deref().unwrap_or("-"),
        clock as f64 / 1000.0,
        edid[126]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn factory_edid_is_valid_and_describes_1080p() {
        validate(&FACTORY).unwrap();
        let text = summary(&FACTORY);
        assert!(text.contains("manufacturer EGA"), "{text}");
        assert!(text.contains("name Elgato"), "{text}");
        assert!(text.contains("preferred 1920x1080 @ 60.00 Hz"), "{text}");
        assert!(text.contains("1 extension block"), "{text}");
    }

    #[test]
    fn checksum_errors_are_reported_and_fixable() {
        let mut edid = FACTORY;
        edid[100] ^= 0x01;
        let error = validate(&edid).unwrap_err();
        assert!(error.contains("block 0 checksum"), "{error}");
        fix_checksums(&mut edid);
        validate(&edid).unwrap();
    }

    #[test]
    fn wrong_size_and_header_are_rejected() {
        assert!(validate(&FACTORY[..128]).is_err());
        let mut edid = FACTORY;
        edid[0] = 1;
        assert!(validate(&edid).unwrap_err().contains("header"));
    }
}
