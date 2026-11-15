//! Terminal setup and teardown.
//!
//! The guard enables raw mode and the alternate screen on construction and
//! restores both when dropped. A panic hook restores the terminal before the
//! previous hook runs, so a panic does not leave the user's shell broken.

use std::io::{self, Stdout};
use std::sync::atomic::{AtomicBool, Ordering};

use crossterm::cursor::{Hide, Show};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

/// The concrete terminal type used by the TUI.
pub type TuiTerminal = Terminal<CrosstermBackend<Stdout>>;

/// Whether the terminal is currently in TUI mode.
///
/// The panic hook cannot borrow the guard, so it shares this flag instead. It
/// also makes [`restore`] idempotent: only the first caller touches the
/// terminal.
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Whether the panic hook has been installed.
static HOOK_INSTALLED: AtomicBool = AtomicBool::new(false);

/// Owns the terminal and restores it on drop.
#[derive(Debug)]
pub struct TerminalGuard {
    terminal: TuiTerminal,
}

impl TerminalGuard {
    /// Enables raw mode and the alternate screen, then creates the terminal.
    pub fn init() -> io::Result<Self> {
        enable_raw_mode()?;

        let mut stdout = io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen, Hide) {
            let _ = disable_raw_mode();
            return Err(error);
        }

        ACTIVE.store(true, Ordering::SeqCst);
        install_panic_hook();

        let backend = CrosstermBackend::new(stdout);
        let terminal = match Terminal::new(backend) {
            Ok(terminal) => terminal,
            Err(error) => {
                restore();
                return Err(error);
            }
        };

        Ok(Self { terminal })
    }

    /// Returns the terminal for drawing.
    pub fn terminal(&mut self) -> &mut TuiTerminal {
        &mut self.terminal
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore();
    }
}

/// Restores the terminal exactly once.
fn restore() {
    if !ACTIVE.swap(false, Ordering::SeqCst) {
        return;
    }

    // Order matters: leave raw mode first, then leave the alternate screen.
    let _ = disable_raw_mode();
    let mut stdout = io::stdout();
    let _ = execute!(stdout, LeaveAlternateScreen, Show);
}

/// Chains a terminal-restoring hook in front of the existing panic hook.
fn install_panic_hook() {
    if HOOK_INSTALLED.swap(true, Ordering::SeqCst) {
        return;
    }

    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_is_noop_when_inactive() {
        // The test binary never calls `TerminalGuard::init`, so this must not
        // touch the terminal or panic.
        restore();
        restore();
    }
}
