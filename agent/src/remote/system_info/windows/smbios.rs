//! The firmware's SMBIOS tables: who made the computer, its model and serial
//! number, and the BIOS or UEFI version. A self-built computer often leaves
//! the System structure's model blank, so the motherboard's is read too.
use anyhow::{Context, ensure};
use windows::Win32::System::SystemInformation::{GetSystemFirmwareTable, RSMB};

/// What the BIOS (type 0), System (type 1) and Baseboard (type 2)
/// structures say.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Firmware {
    pub(super) bios_vendor: Option<String>,
    pub(super) bios_version: Option<String>,
    pub(super) bios_date: Option<String>,
    pub(super) manufacturer: Option<String>,
    pub(super) product: Option<String>,
    pub(super) serial_number: Option<String>,
    pub(super) board_manufacturer: Option<String>,
    pub(super) board_product: Option<String>,
}

impl Firmware {
    /// Such as `American Megatrends Inc. 1.40 (03/14/2023)`, from the parts
    /// the firmware names.
    pub(super) fn version(&self) -> Option<String> {
        let mut text = [&self.bios_vendor, &self.bios_version]
            .into_iter()
            .flatten()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(" ");
        if let Some(date) = &self.bios_date {
            text = if text.is_empty() {
                date.clone()
            } else {
                format!("{text} ({date})")
            };
        }
        (!text.is_empty()).then_some(text)
    }
}

/// Reads the raw SMBIOS data Windows keeps from boot.
pub(super) fn read() -> anyhow::Result<Firmware> {
    // SAFETY: no buffer asks for the size.
    let size = unsafe { GetSystemFirmwareTable(RSMB, 0, None) };
    ensure!(
        size > 0,
        "GetSystemFirmwareTable: {}",
        std::io::Error::last_os_error()
    );
    let mut buffer = vec![0u8; size as usize];
    // SAFETY: the buffer is as large as the table.
    let written = unsafe { GetSystemFirmwareTable(RSMB, 0, Some(&mut buffer)) };
    ensure!(
        written > 0 && written as usize <= buffer.len(),
        "GetSystemFirmwareTable: {}",
        std::io::Error::last_os_error()
    );
    buffer.truncate(written as usize);
    // RawSMBIOSData: four version bytes, the table's length, then the table.
    let length = buffer
        .get(4..8)
        .map(|bytes| u32::from_le_bytes(bytes.try_into().expect("four bytes")) as usize)
        .context("the SMBIOS data is truncated")?;
    let table = buffer
        .get(8..8 + length)
        .or_else(|| buffer.get(8..))
        .context("the SMBIOS data is truncated")?;
    Ok(parse(table))
}

/// Reads the first BIOS, System and Baseboard structures of an SMBIOS table.
pub(super) fn parse(table: &[u8]) -> Firmware {
    let mut firmware = Firmware::default();
    let (mut bios, mut system, mut board) = (false, false, false);
    let mut offset = 0;
    while let Some(&[kind, length]) = table.get(offset..offset + 2) {
        let length = usize::from(length);
        let Some(formatted) = table.get(offset..offset + length).filter(|_| length >= 4) else {
            break;
        };
        // The strings follow the formatted area, each NUL-terminated, and
        // end with an empty one.
        let strings_start = offset + length;
        let Some(end) = table[strings_start..]
            .windows(2)
            .position(|pair| pair == [0, 0])
        else {
            break;
        };
        let strings: Vec<&[u8]> = table[strings_start..strings_start + end]
            .split(|byte| *byte == 0)
            .collect();
        let text = |field: usize| -> Option<String> {
            let index = usize::from(*formatted.get(field)?);
            let bytes = strings.get(index.checked_sub(1)?)?;
            meaningful(&String::from_utf8_lossy(bytes))
        };
        match kind {
            0 if !bios => {
                bios = true;
                firmware.bios_vendor = text(4);
                firmware.bios_version = text(5);
                firmware.bios_date = text(8);
            }
            1 if !system => {
                system = true;
                firmware.manufacturer = text(4);
                firmware.product = text(5);
                firmware.serial_number = text(7);
            }
            2 if !board => {
                board = true;
                firmware.board_manufacturer = text(4);
                firmware.board_product = text(5);
            }
            // End of table.
            127 => break,
            _ => {}
        }
        offset = strings_start + end + 2;
    }
    firmware
}

/// Placeholders firmware ships when the OEM didn't fill a field in.
const PLACEHOLDERS: &[&str] = &[
    "to be filled by o.e.m.",
    "to be filled by oem",
    "o.e.m.",
    "oem",
    "default string",
    "system manufacturer",
    "system product name",
    "system serial number",
    "system version",
    "not specified",
    "not applicable",
    "not available",
    "none",
    "n/a",
    "na",
    "0",
    "00000000",
    "123456789",
    "1234567890",
    "invalid",
    "unknown",
    "x.x",
];

/// The trimmed text, or `None` if it is empty or an OEM placeholder.
pub(super) fn meaningful(text: &str) -> Option<String> {
    let text = text.trim().trim_matches('\0').trim();
    let lower = text.to_lowercase();
    (!text.is_empty() && !PLACEHOLDERS.contains(&lower.as_str())).then(|| text.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An SMBIOS structure with its formatted area and strings.
    fn structure(kind: u8, fields: &[u8], strings: &[&str]) -> Vec<u8> {
        let mut bytes = vec![kind, (4 + fields.len()) as u8, 0, 0];
        bytes.extend_from_slice(fields);
        for text in strings {
            bytes.extend_from_slice(text.as_bytes());
            bytes.push(0);
        }
        if strings.is_empty() {
            bytes.push(0);
        }
        bytes.push(0);
        bytes
    }

    #[test]
    fn bios_and_system_structures_are_read() {
        let mut table = structure(
            0,
            // Vendor 1, version 2, starting segment, release date 3.
            &[1, 2, 0, 0, 3, 0],
            &["American Megatrends Inc.", "1.40", "03/14/2023"],
        );
        // An unrelated structure without strings.
        table.extend(structure(32, &[0; 7], &[]));
        table.extend(structure(
            1,
            // Manufacturer 1, product 2, version 3, serial 4.
            &[1, 2, 3, 4],
            &[
                "Micro-Star International Co., Ltd.",
                "MS-7C02",
                "1.0",
                "To be filled by O.E.M.",
            ],
        ));
        // A second System structure doesn't override the first.
        table.extend(structure(1, &[1, 1, 1, 1], &["Other"]));
        table.extend(structure(
            2,
            // Manufacturer 1, product 2, version 3, serial 4.
            &[1, 2, 3, 4],
            &[
                "ASUSTeK COMPUTER INC.",
                "ROG STRIX Z490-E GAMING",
                "Rev 1.xx",
                "200",
            ],
        ));
        table.extend(structure(127, &[], &[]));
        let firmware = parse(&table);
        assert_eq!(
            firmware,
            Firmware {
                bios_vendor: Some("American Megatrends Inc.".into()),
                bios_version: Some("1.40".into()),
                bios_date: Some("03/14/2023".into()),
                manufacturer: Some("Micro-Star International Co., Ltd.".into()),
                product: Some("MS-7C02".into()),
                serial_number: None,
                board_manufacturer: Some("ASUSTeK COMPUTER INC.".into()),
                board_product: Some("ROG STRIX Z490-E GAMING".into()),
            }
        );
        assert_eq!(
            firmware.version().as_deref(),
            Some("American Megatrends Inc. 1.40 (03/14/2023)")
        );
    }

    #[test]
    fn truncated_tables_and_missing_strings_are_safe() {
        let table = structure(1, &[1, 9, 0, 2], &["Dell Inc.", "ABC1234"]);
        let firmware = parse(&table);
        assert_eq!(firmware.manufacturer.as_deref(), Some("Dell Inc."));
        assert_eq!(firmware.product, None);
        assert_eq!(firmware.serial_number.as_deref(), Some("ABC1234"));
        assert_eq!(parse(&table[..table.len() - 3]), Firmware::default());
        assert_eq!(parse(&[1]), Firmware::default());
        assert_eq!(parse(&[1, 2, 0, 0]), Firmware::default());
        assert_eq!(Firmware::default().version(), None);
        let dated = Firmware {
            bios_date: Some("01/01/2020".into()),
            ..Default::default()
        };
        assert_eq!(dated.version().as_deref(), Some("01/01/2020"));
    }

    #[test]
    fn placeholders_are_missing_values() {
        for placeholder in [
            "To be filled by O.E.M.",
            "Default string",
            " System Serial Number ",
            "System Product Name",
            "Not Specified",
            "None",
            "0",
            "123456789",
            "",
            "   ",
        ] {
            assert_eq!(meaningful(placeholder), None, "{placeholder:?}");
        }
        assert_eq!(meaningful(" 5CD1234XYZ ").as_deref(), Some("5CD1234XYZ"));
    }
}
