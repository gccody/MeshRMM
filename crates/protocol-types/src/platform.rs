use serde::{Deserialize, Serialize};

/// The operating system an Agent runs on. It decides the controls a viewer
/// offers, such as Ctrl+Alt+Del and Safe Mode on Windows only, and the
/// scripts and sessions the server lets a device have.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DevicePlatform {
    Windows,
    Macos,
    Linux,
}

impl DevicePlatform {
    /// The platform this Agent is built for.
    pub const fn current() -> Self {
        if cfg!(target_os = "macos") {
            Self::Macos
        } else if cfg!(target_os = "linux") {
            Self::Linux
        } else {
            Self::Windows
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Windows => "windows",
            Self::Macos => "macos",
            Self::Linux => "linux",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "windows" => Some(Self::Windows),
            "macos" => Some(Self::Macos),
            "linux" => Some(Self::Linux),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Windows => "Windows",
            Self::Macos => "macOS",
            Self::Linux => "Linux",
        }
    }

    /// Whether the platform has a background desktop a technician can work
    /// on without the user seeing it; see `docs/background-mode.md`.
    pub fn has_background_desktop(self) -> bool {
        self == Self::Windows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for platform in [
            DevicePlatform::Windows,
            DevicePlatform::Macos,
            DevicePlatform::Linux,
        ] {
            assert_eq!(DevicePlatform::parse(platform.as_str()), Some(platform));
            assert_eq!(
                serde_json::to_value(platform).unwrap(),
                serde_json::Value::String(platform.as_str().to_owned())
            );
        }
        assert_eq!(DevicePlatform::parse("windows-x64"), None);
    }
}
