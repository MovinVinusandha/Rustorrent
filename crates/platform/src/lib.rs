//! OS-specific glue shared by the engine and daemon.

/// Operating system family detected at compile time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    /// Linux.
    Linux,
    /// Windows.
    Windows,
    /// macOS.
    MacOs,
    /// Any other target.
    Other,
}

/// Returns the operating system family for the current build target.
pub const fn current_os() -> Os {
    #[cfg(target_os = "linux")]
    {
        Os::Linux
    }
    #[cfg(target_os = "windows")]
    {
        Os::Windows
    }
    #[cfg(target_os = "macos")]
    {
        Os::MacOs
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
    {
        Os::Other
    }
}
