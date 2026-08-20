use anyhow::{Context, bail};

#[derive(Debug)]
pub struct AvccAccessUnit {
    pub data: Vec<u8>,
    pub sequence_parameter_set: Option<Vec<u8>>,
    pub picture_parameter_set: Option<Vec<u8>>,
}

/// Converts the Agent's Annex-B access unit into the four-byte length-prefixed
/// representation required by Apple's CoreMedia H.264 format description.
pub fn annex_b_to_avcc(data: &[u8]) -> anyhow::Result<AvccAccessUnit> {
    let units = annex_b_units(data);
    if units.is_empty() {
        bail!("H.264 access unit contains no Annex-B NAL units");
    }

    let mut output = Vec::with_capacity(data.len());
    let mut sequence_parameter_set = None;
    let mut picture_parameter_set = None;
    for unit in units {
        let length = u32::try_from(unit.len()).context("H.264 NAL unit exceeds 4 GiB")?;
        output.extend_from_slice(&length.to_be_bytes());
        output.extend_from_slice(unit);
        match unit[0] & 0x1f {
            7 => sequence_parameter_set = Some(unit.to_vec()),
            8 => picture_parameter_set = Some(unit.to_vec()),
            _ => {}
        }
    }

    Ok(AvccAccessUnit {
        data: output,
        sequence_parameter_set,
        picture_parameter_set,
    })
}

fn annex_b_units(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut index = 0;
    while index + 3 <= data.len() {
        let prefix = if data[index..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if data[index..].starts_with(&[0, 0, 1]) {
            3
        } else {
            index += 1;
            continue;
        };
        starts.push((index, prefix));
        index += prefix;
    }

    starts
        .iter()
        .enumerate()
        .filter_map(|(position, &(start, prefix))| {
            let unit_start = start + prefix;
            let mut unit_end = starts
                .get(position + 1)
                .map_or(data.len(), |&(next, _)| next);
            while unit_end > unit_start && data[unit_end - 1] == 0 {
                unit_end -= 1;
            }
            (unit_start < unit_end).then_some(&data[unit_start..unit_end])
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_annex_b_and_extracts_parameter_sets() {
        let annex_b = [
            0, 0, 0, 1, 0x67, 0x64, 0, 0x1f, 0, 0, 1, 0x68, 0xee, 0, 0, 0, 1, 0x65, 0xaa,
        ];
        let converted = annex_b_to_avcc(&annex_b).unwrap();
        assert_eq!(
            converted.sequence_parameter_set,
            Some(vec![0x67, 0x64, 0, 0x1f])
        );
        assert_eq!(converted.picture_parameter_set, Some(vec![0x68, 0xee]));
        assert_eq!(
            converted.data,
            [
                0, 0, 0, 4, 0x67, 0x64, 0, 0x1f, 0, 0, 0, 2, 0x68, 0xee, 0, 0, 0, 2, 0x65, 0xaa,
            ]
        );
    }

    #[test]
    fn rejects_non_annex_b_data() {
        assert!(annex_b_to_avcc(&[1, 2, 3]).is_err());
    }
}
