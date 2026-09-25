//! Release-asset platform detection and filtering.
//!
//! Asset names are inconsistent: `foo-linux-x86_64.tar.gz`,
//! `foo_1.2.0_amd64.deb`, `foo-x86_64-pc-windows-msvc.zip`, `Bar-1.0-arm64.dmg`.
//! We scan the filename for OS and CPU tokens (Rust target triples included)
//! and match the result against the filters set in Settings.
//!
//! Both an OS and an arch are required, so `foo-linux.tar.gz` (any arch) is
//! skipped rather than guessed at.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Os {
    Darwin,
    Windows,
    Linux,
    FreeBsd,
}

impl Os {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Darwin => "darwin",
            Self::Windows => "windows",
            Self::Linux => "linux",
            Self::FreeBsd => "freebsd",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Darwin => "macOS",
            Self::Windows => "Windows",
            Self::Linux => "Linux",
            Self::FreeBsd => "FreeBSD",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Arch {
    Arm64,
    X64,
    X86,
    Armv7,
    Armv6,
    Ppc64le,
    Riscv64,
    Universal,
}

impl Arch {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Arm64 => "arm64",
            Self::X64 => "x64",
            Self::X86 => "x86",
            Self::Armv7 => "armv7",
            Self::Armv6 => "armv6",
            Self::Ppc64le => "ppc64le",
            Self::Riscv64 => "riscv64",
            Self::Universal => "universal",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Arm64 => "arm64",
            Self::X64 => "x86_64",
            Self::X86 => "x86 (32-bit)",
            Self::Armv7 => "armv7",
            Self::Armv6 => "armv6",
            Self::Ppc64le => "ppc64le",
            Self::Riscv64 => "riscv64",
            Self::Universal => "universal",
        }
    }
}

/// A concrete platform: an OS plus a CPU architecture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Platform {
    pub os: Os,
    pub arch: Arch,
}

impl Platform {
    /// Canonical, stable identifier (`darwin-arm64`, `windows-x64`).
    pub fn slug(self) -> String {
        format!("{}-{}", self.os.as_str(), self.arch.as_str())
    }

    pub fn label(self) -> String {
        format!("{} · {}", self.os.label(), self.arch.label())
    }
}

/// A configured platform filter. Any `None` component is a wildcard:
/// `linux` matches every Linux arch, `arm64` matches arm64 everywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Filter {
    pub os: Option<Os>,
    pub arch: Option<Arch>,
}

impl Filter {
    pub fn matches(self, p: Platform) -> bool {
        self.os.is_none_or(|o| o == p.os) && self.arch.is_none_or(|a| a == p.arch)
    }
}

/// Lowercase, split on anything that is not alphanumeric. Splitting rather
/// than substring matching is what keeps `darwin` from looking like `win`.
fn tokens(hay: &str) -> impl Iterator<Item = &str> {
    hay.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
}

fn has_token(hay: &str, wanted: &[&str]) -> bool {
    tokens(hay).any(|t| wanted.contains(&t))
}

/// Detect the operating system from a filename (or a filter string).
pub fn detect_os(hay: &str) -> Option<Os> {
    let h = hay.to_ascii_lowercase();
    if has_token(
        &h,
        &["windows", "win", "win32", "win64", "msvc", "mingw", "cygwin", "uwp", "msys"],
    ) || h.ends_with(".exe")
        || h.ends_with(".msi")
        || h.ends_with(".msix")
    {
        return Some(Os::Windows);
    }
    if has_token(
        &h,
        &["darwin", "macos", "macosx", "osx", "mac", "apple", "macho", "dmg", "pkg"],
    ) || h.ends_with(".dmg")
        || h.ends_with(".pkg")
    {
        return Some(Os::Darwin);
    }
    if has_token(&h, &["freebsd"]) {
        return Some(Os::FreeBsd);
    }
    if has_token(
        &h,
        &["linux", "musl", "glibc", "gnu", "deb", "rpm", "appimage"],
    ) || h.ends_with(".deb")
        || h.ends_with(".rpm")
        || h.ends_with(".appimage")
    {
        return Some(Os::Linux);
    }
    None
}

/// Detect the CPU architecture from a filename (or a filter string).
/// Order matters: `x86_64` and `x64` must be checked before the bare `x86`.
pub fn detect_arch(hay: &str) -> Option<Arch> {
    let h = hay.to_ascii_lowercase();
    if has_token(&h, &["universal", "universal2", "fat", "fatbinary"]) {
        return Some(Arch::Universal);
    }
    if h.contains("aarch64") || h.contains("arm64") || h.contains("armv8") {
        return Some(Arch::Arm64);
    }
    if h.contains("x86_64")
        || h.contains("x86-64")
        || h.contains("x86.64")
        || has_token(&h, &["amd64", "x64", "win64", "intel64", "ia64"])
    {
        return Some(Arch::X64);
    }
    if h.contains("x86_32")
        || h.contains("x86-32")
        || h.contains("x86.32")
        || has_token(
            &h,
            &["i686", "i586", "i486", "i386", "win32", "x86", "386", "32bit", "x32"],
        )
    {
        return Some(Arch::X86);
    }
    if h.contains("armv7") || has_token(&h, &["armhf", "armv7l", "armv7hl", "arm32"]) {
        return Some(Arch::Armv7);
    }
    if h.contains("armv6") || has_token(&h, &["armel", "armv5"]) {
        return Some(Arch::Armv6);
    }
    if h.contains("ppc64le") || h.contains("powerpc64le") {
        return Some(Arch::Ppc64le);
    }
    if h.contains("riscv64") {
        return Some(Arch::Riscv64);
    }
    // generic 32-bit ARM fallback (after the more specific arm64/armv7 checks)
    if has_token(&h, &["arm"]) {
        return Some(Arch::Armv7);
    }
    None
}

/// Classify an asset filename into a platform, if it names both an OS and
/// an architecture. Returns `None` for sources, checksums, signatures and
/// anything else that is not clearly a platform-specific build.
pub fn classify(filename: &str) -> Option<Platform> {
    // asset names are single path components, but be defensive against a
    // weird path from a forge API
    let name = filename.rsplit(['/', '\\']).next().unwrap_or(filename);
    let os = detect_os(name)?;
    let arch = detect_arch(name)?;
    Some(Platform { os, arch })
}

/// Parse a configured filter. `all`/`*`/`any` matches everything. A filter
/// that names neither an OS nor an arch is invalid (`None`).
pub fn parse_filter(s: &str) -> Option<Filter> {
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    if t.eq_ignore_ascii_case("all") || t == "*" || t.eq_ignore_ascii_case("any") {
        return Some(Filter { os: None, arch: None });
    }
    let os = detect_os(t);
    let arch = detect_arch(t);
    if os.is_none() && arch.is_none() {
        return None;
    }
    Some(Filter { os, arch })
}

/// Normalize a user-entered filter, e.g. `win64` becomes `windows-x64` and
/// `Linux X86` becomes `linux-x86` (`all` stays `all`).
pub fn canonical(s: &str) -> Option<String> {
    let f = parse_filter(s)?;
    Some(match (f.os, f.arch) {
        (None, None) => "all".to_string(),
        (Some(o), Some(a)) => format!("{}-{}", o.as_str(), a.as_str()),
        (Some(o), None) => o.as_str().to_string(),
        (None, Some(a)) => a.as_str().to_string(),
    })
}

/// Does `platform` match any of the configured filters?
pub fn matches_any(filters: &[String], platform: Platform) -> bool {
    filters
        .iter()
        .filter_map(|f| parse_filter(f))
        .any(|f| f.matches(platform))
}

/// Human description of a stored/canonical platform slug.
pub fn describe(slug: &str) -> String {
    match parse_filter(slug) {
        Some(Filter { os: None, arch: None }) => "all platforms".to_string(),
        Some(Filter { os: Some(o), arch: Some(a) }) => {
            format!("{} · {}", o.label(), a.label())
        }
        Some(Filter { os: Some(o), arch: None }) => o.label().to_string(),
        Some(Filter { os: None, arch: Some(a) }) => a.label().to_string(),
        None => slug.to_string(),
    }
}

/// The platforms offered as checkboxes in Settings. Users can still type
/// arbitrary filters in the extra field.
pub struct Choice {
    pub slug: &'static str,
    pub label: &'static str,
}

pub const CHOICES: &[Choice] = &[
    Choice { slug: "darwin-arm64", label: "macOS · arm64 (Apple silicon)" },
    Choice { slug: "darwin-x64", label: "macOS · x86_64 (Intel)" },
    Choice { slug: "windows-x64", label: "Windows · x64 (win64)" },
    Choice { slug: "windows-x86", label: "Windows · x86 (win32)" },
    Choice { slug: "windows-arm64", label: "Windows · arm64" },
    Choice { slug: "linux-x64", label: "Linux · x86_64" },
    Choice { slug: "linux-x86", label: "Linux · x86 (i686)" },
    Choice { slug: "linux-arm64", label: "Linux · arm64" },
    Choice { slug: "linux-armv7", label: "Linux · armv7 (armhf)" },
    Choice { slug: "freebsd-x64", label: "FreeBSD · x64" },
];

#[cfg(test)]
mod tests {
    use super::*;

    fn p(os: Os, arch: Arch) -> Platform {
        Platform { os, arch }
    }

    #[test]
    fn rust_target_triples() {
        assert_eq!(
            classify("app-v1.2.3-x86_64-unknown-linux-gnu.tar.gz"),
            Some(p(Os::Linux, Arch::X64))
        );
        assert_eq!(
            classify("app-v1.2.3-aarch64-apple-darwin.tar.gz"),
            Some(p(Os::Darwin, Arch::Arm64))
        );
        assert_eq!(
            classify("app-v1.2.3-x86_64-pc-windows-msvc.zip"),
            Some(p(Os::Windows, Arch::X64))
        );
        assert_eq!(
            classify("app-v1.2.3-x86_64-pc-windows-gnu.zip"),
            Some(p(Os::Windows, Arch::X64))
        );
        assert_eq!(
            classify("app-v1.2.3-i686-unknown-linux-musl.tar.gz"),
            Some(p(Os::Linux, Arch::X86))
        );
    }

    #[test]
    fn common_naming_styles() {
        assert_eq!(classify("foo-linux-x86_64.tar.gz"), Some(p(Os::Linux, Arch::X64)));
        assert_eq!(classify("foo-win64.zip"), Some(p(Os::Windows, Arch::X64)));
        assert_eq!(classify("foo-win32.zip"), Some(p(Os::Windows, Arch::X86)));
        assert_eq!(classify("foo-windows-amd64.exe"), Some(p(Os::Windows, Arch::X64)));
        assert_eq!(classify("Foo-1.0-arm64.dmg"), Some(p(Os::Darwin, Arch::Arm64)));
        assert_eq!(classify("Foo-1.0-x64.dmg"), Some(p(Os::Darwin, Arch::X64)));
        assert_eq!(classify("foo_1.2.0_amd64.deb"), Some(p(Os::Linux, Arch::X64)));
        assert_eq!(classify("foo-1.2.386-linux.tar.gz"), Some(p(Os::Linux, Arch::X86)));
        assert_eq!(classify("go1.21.linux-arm64.tar.gz"), Some(p(Os::Linux, Arch::Arm64)));
        assert_eq!(classify("app-linux-armv7.tar.gz"), Some(p(Os::Linux, Arch::Armv7)));
        assert_eq!(classify("app-linux-armhf.tar.gz"), Some(p(Os::Linux, Arch::Armv7)));
    }

    #[test]
    fn darwin_does_not_look_like_win() {
        assert_eq!(detect_os("darwin-arm64.tar.gz"), Some(Os::Darwin));
        assert_eq!(classify("foo-macos-x86_64.zip"), Some(p(Os::Darwin, Arch::X64)));
    }

    #[test]
    fn ambiguous_and_non_binary_assets_are_skipped() {
        // no arch
        assert_eq!(classify("foo-linux.tar.gz"), None);
        // no os
        assert_eq!(classify("foo-arm64.tar.gz"), None);
        // checksums / signatures / sources
        assert_eq!(classify("SHA256SUMS"), None);
        assert_eq!(classify("app-1.2.3.tar.gz"), None);
        assert_eq!(classify("source.zip"), None);
        // Android packages are not one of our OS targets
        assert_eq!(classify("app-arm64-v8a.apk"), None);
        assert_eq!(classify("app-v1.2.3-linux-amd64.sha256"), Some(p(Os::Linux, Arch::X64)));
    }

    #[test]
    fn classify_ignores_path_components() {
        assert_eq!(
            classify("releases/v1.0/foo_linux_x86_64.tar.gz"),
            Some(p(Os::Linux, Arch::X64))
        );
        assert_eq!(
            classify("releases/win64/foo-win64.exe"),
            Some(p(Os::Windows, Arch::X64))
        );
    }

    #[test]
    fn filters_parse_and_normalize() {
        assert_eq!(canonical("win64").as_deref(), Some("windows-x64"));
        assert_eq!(canonical("win32").as_deref(), Some("windows-x86"));
        assert_eq!(canonical("Linux X86").as_deref(), Some("linux-x86"));
        assert_eq!(canonical("linux x86_64").as_deref(), Some("linux-x64"));
        assert_eq!(canonical("arm64-osx/darwin").as_deref(), Some("darwin-arm64"));
        assert_eq!(canonical("all").as_deref(), Some("all"));
        assert_eq!(canonical("*").as_deref(), Some("all"));
        assert_eq!(canonical("linux").as_deref(), Some("linux"));
        assert_eq!(canonical("arm64").as_deref(), Some("arm64"));
        assert_eq!(canonical("nonsense"), None);
    }

    #[test]
    fn filters_match_the_right_platforms() {
        let filters = vec!["darwin-arm64".to_string(), "linux-x64".to_string()];
        assert!(matches_any(&filters, p(Os::Darwin, Arch::Arm64)));
        assert!(matches_any(&filters, p(Os::Linux, Arch::X64)));
        assert!(!matches_any(&filters, p(Os::Darwin, Arch::X64)));
        assert!(!matches_any(&filters, p(Os::Windows, Arch::X64)));
        // OS-wide filter
        let linux = vec!["linux".to_string()];
        assert!(matches_any(&linux, p(Os::Linux, Arch::Arm64)));
        assert!(!matches_any(&linux, p(Os::Darwin, Arch::Arm64)));
        // all
        let all = vec!["all".to_string()];
        assert!(matches_any(&all, p(Os::Windows, Arch::X86)));
        // arch-wide filter
        let arm = vec!["arm64".to_string()];
        assert!(matches_any(&arm, p(Os::Windows, Arch::Arm64)));
        assert!(!matches_any(&arm, p(Os::Linux, Arch::X64)));
        // empty = nothing
        assert!(!matches_any(&[], p(Os::Linux, Arch::X64)));
        // invalid tokens are ignored
        assert!(!matches_any(&["lolwut".to_string()], p(Os::Linux, Arch::X64)));
    }

    #[test]
    fn filter_config_used_by_user() {
        // the user's stated selection: arm64 and linux x86_64 only
        let filters = vec!["arm64".to_string(), "linux x86_64".to_string()];
        assert_eq!(canonical("arm64").as_deref(), Some("arm64"));
        assert_eq!(canonical("linux x86_64").as_deref(), Some("linux-x64"));
        assert!(matches_any(&filters, p(Os::Darwin, Arch::Arm64)));
        assert!(matches_any(&filters, p(Os::Linux, Arch::Arm64)));
        assert!(matches_any(&filters, p(Os::Linux, Arch::X64)));
        assert!(!matches_any(&filters, p(Os::Windows, Arch::X64)));
        assert!(!matches_any(&filters, p(Os::Darwin, Arch::X64)));
    }
}
