//! Turning what the Windows Update Agent reports into the protocol's terms:
//! classifications, KB articles, dates, sizes and error codes.
use meshrmm_protocol::UpdateCategory;

/// An update's category from the ID of its `UpdateClassification`
/// category. These IDs are the same on every Windows release and language.
pub(super) fn category_of_classification(id: &str) -> Option<UpdateCategory> {
    let id = id.trim().trim_start_matches('{').trim_end_matches('}');
    let category = match id.to_ascii_uppercase().as_str() {
        "0FA1201D-4330-4FA8-8AE9-B877473B6441" => UpdateCategory::Security,
        "E6CF1350-C01B-414D-A61F-263D14D133B4" => UpdateCategory::Critical,
        "E0789628-CE08-4437-BE74-2495B842F43B" => UpdateCategory::Definition,
        "EBFC1FC5-71A4-4F7B-9ACA-3B9A503104A0" => UpdateCategory::Driver,
        "B54E7D24-7ADD-428F-8B75-90A396FA584F" => UpdateCategory::FeaturePack,
        "68C5B0A3-D1A6-4553-AE49-01D3A7827828" => UpdateCategory::ServicePack,
        "B4832BD8-E735-4761-8DAF-37F882276DAB" => UpdateCategory::Tool,
        "28BC880E-0592-4CBF-8F95-C79B17911D5F" => UpdateCategory::UpdateRollup,
        "3689BDC8-B205-4AF4-8D4A-A63924C5E9D5" => UpdateCategory::Upgrade,
        "CD5FFD1E-E932-4E3A-BF74-18BF0B1BBD83" => UpdateCategory::Other,
        _ => return None,
    };
    Some(category)
}

/// `KB5034441` from the article numbers Windows Update lists, joined when
/// an update names several, or `None` when it names none.
pub(super) fn knowledge_base(articles: &[String]) -> Option<String> {
    let articles = articles
        .iter()
        .map(|article| article.trim())
        .filter(|article| !article.is_empty())
        .map(|article| {
            if article.len() > 2 && article[..2].eq_ignore_ascii_case("KB") {
                format!("KB{}", &article[2..])
            } else {
                format!("KB{article}")
            }
        })
        .collect::<Vec<_>>();
    (!articles.is_empty()).then(|| articles.join(", "))
}

/// Unix milliseconds of an OLE automation date: days since 30 December
/// 1899, with the time of day as the fraction. `None` for the zero date
/// Windows Update gives an update it has no date for.
pub(super) fn ole_date_unix_ms(date: f64) -> Option<i64> {
    /// The OLE date of 1 January 1970.
    const UNIX_EPOCH: f64 = 25_569.0;
    const DAY_MS: f64 = 86_400_000.0;
    if !date.is_finite() || date <= 1.0 {
        return None;
    }
    let milliseconds = ((date - UNIX_EPOCH) * DAY_MS).round();
    (milliseconds.abs() < i64::MAX as f64).then_some(milliseconds as i64)
}

/// Why Windows Update failed, in words, with its error code.
pub(super) fn error_message(code: i32, system_message: &str) -> String {
    let hex = format!("0x{:08X}", code as u32);
    let known = match code as u32 {
        0x8024_402C | 0x8007_2EE7 => {
            Some("the update service's name couldn't be resolved; check the network connection")
        }
        0x8024_0438 | 0x8007_2EFD | 0x8007_2EFE => {
            Some("the update service couldn't be reached; check the network connection")
        }
        0x8024_401C | 0x8007_2EE2 => Some("the update service didn't answer in time"),
        0x8024_4022 => Some("the update service is unavailable"),
        0x8024_002E => Some("access to Windows Update is turned off by policy"),
        0x8007_0422 | 0x8024_001E => Some("the Windows Update service is turned off or stopping"),
        0x8024_0016 => Some("another installation is in progress"),
        0x8024_000B => Some("the operation was cancelled"),
        0x8024_0022 => Some("every update failed to install"),
        0x8024_A000 => Some("the Automatic Updates service isn't running"),
        0x8024_6007 => Some("the update wasn't downloaded"),
        0x8007_0005 => Some("access was denied"),
        0x8007_0070 => Some("the disk is full"),
        _ => None,
    };
    let system_message = system_message.trim().trim_end_matches('.');
    match known {
        Some(text) => format!("{text} ({hex})"),
        None if !system_message.is_empty() => format!("{system_message} ({hex})"),
        None => format!("Windows Update error {hex}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifications_map_by_their_ids_in_any_case() {
        assert_eq!(
            category_of_classification("0fa1201d-4330-4fa8-8ae9-b877473b6441"),
            Some(UpdateCategory::Security)
        );
        assert_eq!(
            category_of_classification("{E0789628-CE08-4437-BE74-2495B842F43B}"),
            Some(UpdateCategory::Definition)
        );
        assert_eq!(
            category_of_classification("CD5FFD1E-E932-4E3A-BF74-18BF0B1BBD83"),
            Some(UpdateCategory::Other)
        );
        assert_eq!(
            category_of_classification("6964AAB4-C5B5-43BD-A17D-FFB4346A8E1D"),
            None,
            "a product, not a classification"
        );
    }

    #[test]
    fn knowledge_base_articles_get_their_prefix() {
        assert_eq!(
            knowledge_base(&["5034441".into()]).as_deref(),
            Some("KB5034441")
        );
        assert_eq!(
            knowledge_base(&["kb2267602".into(), " 890830 ".into()]).as_deref(),
            Some("KB2267602, KB890830")
        );
        assert_eq!(knowledge_base(&[String::new()]), None);
        assert_eq!(knowledge_base(&[]), None);
    }

    #[test]
    fn ole_dates_become_unix_milliseconds() {
        assert_eq!(ole_date_unix_ms(25_569.0), Some(0));
        // 2024-01-09 12:00 UTC.
        assert_eq!(ole_date_unix_ms(45_300.5), Some(1_704_801_600_000));
        assert_eq!(ole_date_unix_ms(0.0), None);
        assert_eq!(ole_date_unix_ms(f64::NAN), None);
    }

    #[test]
    fn errors_name_their_code() {
        assert_eq!(
            error_message(0x8024_402C_u32 as i32, ""),
            "the update service's name couldn't be resolved; check the network connection (0x8024402C)"
        );
        assert_eq!(
            error_message(0x8000_4005_u32 as i32, "Unspecified error\r\n"),
            "Unspecified error (0x80004005)"
        );
        assert_eq!(
            error_message(0x8024_FFFF_u32 as i32, ""),
            "Windows Update error 0x8024FFFF"
        );
    }
}
