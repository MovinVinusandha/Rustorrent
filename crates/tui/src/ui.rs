//! Pure rendering for the TUI.
//!
//! Everything in this module takes [`AppState`] by reference and draws into a
//! [`Frame`]. No I/O happens here, so the drawing code is easy to test with
//! ratatui's [`TestBackend`](ratatui::backend::TestBackend).

use bt_client::{TorrentState, TorrentSummary};
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Cell, Clear, Paragraph, Row, Table, TableState, Wrap},
};

use crate::app::AppState;

/// Draws one full frame.
pub fn draw(frame: &mut Frame, state: &AppState) {
    let area = frame.area();
    let [header_area, body_area, footer_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(3),
    ])
    .areas(area);

    draw_header(frame, header_area, state);

    if state.torrents.is_empty() {
        draw_empty(frame, body_area);
    } else {
        draw_torrents(frame, body_area, state);
    }

    draw_footer(frame, footer_area, state);

    if state.show_help {
        draw_help(frame, area);
    }
}

fn draw_header(frame: &mut Frame, area: Rect, state: &AppState) {
    let stats = &state.stats;
    let line = Line::from(vec![
        Span::styled(
            format!(" bt-app {} ", env!("CARGO_PKG_VERSION")),
            Style::new()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            "  down {}  up {}  torrents {} ({})  peers {}",
            format_speed(stats.download_bps),
            format_speed(stats.upload_bps),
            stats.torrents_total,
            stats.torrents_active,
            stats.peers_connected,
        )),
    ]);

    frame.render_widget(Paragraph::new(line), area);
}

fn draw_empty(frame: &mut Frame, area: Rect) {
    let block = Block::bordered().title(" Torrents ");
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let hint = Paragraph::new(vec![
        Line::from("No torrents yet."),
        Line::from(""),
        Line::from("Press ? for key bindings."),
    ])
    .alignment(Alignment::Center)
    .wrap(Wrap { trim: true });

    // Center the hint vertically inside the block.
    let [_, middle, _] = Layout::vertical([
        Constraint::Fill(1),
        Constraint::Length(3),
        Constraint::Fill(1),
    ])
    .areas(inner);

    frame.render_widget(hint, middle);
}

fn draw_torrents(frame: &mut Frame, area: Rect, state: &AppState) {
    let header = Row::new(vec![
        Cell::from("Name"),
        Cell::from("State"),
        Cell::from("Done"),
        Cell::from("Size"),
        Cell::from("Down"),
        Cell::from("Up"),
        Cell::from("Peers"),
        Cell::from("ETA"),
    ])
    .style(Style::new().add_modifier(Modifier::BOLD));

    let widths = [
        Constraint::Min(18),
        Constraint::Length(11),
        Constraint::Length(6),
        Constraint::Length(9),
        Constraint::Length(10),
        Constraint::Length(10),
        Constraint::Length(5),
        Constraint::Length(7),
    ];

    let rows = state.torrents.iter().map(torrent_row);
    let table = Table::new(rows, widths)
        .header(header)
        .block(Block::bordered().title(" Torrents "))
        .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED))
        .highlight_symbol("> ");

    let selected = if state.torrents.is_empty() {
        None
    } else {
        Some(state.selected.min(state.torrents.len() - 1))
    };
    let mut table_state = TableState::default().with_selected(selected);

    frame.render_stateful_widget(table, area, &mut table_state);
}

fn torrent_row(torrent: &TorrentSummary) -> Row<'_> {
    Row::new(vec![
        Cell::from(torrent.name.as_str()),
        Cell::from(state_label(torrent.state)).style(state_style(torrent.state)),
        Cell::from(format_progress(torrent.progress)),
        Cell::from(format_bytes(torrent.total_bytes)),
        Cell::from(format_speed(torrent.download_bps)),
        Cell::from(format_speed(torrent.upload_bps)),
        Cell::from(torrent.peers_connected.to_string()),
        Cell::from(format_eta(torrent.eta_secs)),
    ])
}

fn draw_footer(frame: &mut Frame, area: Rect, state: &AppState) {
    let limits = Line::from(vec![
        Span::styled("limits ", Style::new().add_modifier(Modifier::BOLD)),
        Span::raw(format!(
            "down {}  up {}",
            format_limit(state.limits.download_bps),
            format_limit(state.limits.upload_bps),
        )),
    ]);

    let status = match &state.status {
        Some(message) => Line::from(Span::styled(
            format!("status {message}"),
            Style::new().fg(Color::Yellow),
        )),
        None => Line::from("status ok"),
    };

    let keys = Line::from(Span::styled(
        "q quit  ? help  p pause/resume  j/k move",
        Style::new().fg(Color::DarkGray),
    ));

    let footer = Paragraph::new(vec![limits, status, keys]).wrap(Wrap { trim: true });
    frame.render_widget(footer, area);
}

fn draw_help(frame: &mut Frame, area: Rect) {
    let popup = centered(area, 56, 60);
    frame.render_widget(Clear, popup);

    let lines = vec![
        Line::from(Span::styled(
            "Key bindings",
            Style::new().add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from("  q, Ctrl-C   Quit"),
        Line::from("  ?           Toggle this help"),
        Line::from("  Esc         Close help"),
        Line::from("  Up, k       Previous torrent"),
        Line::from("  Down, j     Next torrent"),
        Line::from("  p           Pause or resume selected"),
        Line::from(""),
        Line::from(Span::styled(
            "Press ? or Esc to close",
            Style::new().fg(Color::DarkGray),
        )),
    ];

    let help = Paragraph::new(lines)
        .block(Block::bordered().title(" Help "))
        .wrap(Wrap { trim: false });

    frame.render_widget(help, popup);
}

/// Returns a rectangle centered in `area` with the given percentage size.
fn centered(area: Rect, percent_x: u16, percent_y: u16) -> Rect {
    let vertical = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .split(area);

    Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(vertical[1])[1]
}

/// Human-readable lifecycle label.
pub fn state_label(state: TorrentState) -> &'static str {
    match state {
        TorrentState::Queued => "Queued",
        TorrentState::Checking => "Checking",
        TorrentState::Downloading => "Downloading",
        TorrentState::Seeding => "Seeding",
        TorrentState::Paused => "Paused",
        TorrentState::Error => "Error",
    }
}

fn state_style(state: TorrentState) -> Style {
    let color = match state {
        TorrentState::Queued => Color::Gray,
        TorrentState::Checking => Color::Blue,
        TorrentState::Downloading => Color::Green,
        TorrentState::Seeding => Color::Cyan,
        TorrentState::Paused => Color::Yellow,
        TorrentState::Error => Color::Red,
    };

    Style::new().fg(color)
}

/// Formats a byte count with binary units.
pub fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;

    let value = bytes as f64;
    if value < KIB {
        return format!("{bytes} B");
    }

    let units = ["KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut scaled = value / KIB;
    let mut unit = 0;

    while scaled >= KIB && unit + 1 < units.len() {
        scaled /= KIB;
        unit += 1;
    }

    format!("{scaled:.1} {}", units[unit])
}

/// Formats a transfer rate in bytes per second.
pub fn format_speed(bytes_per_second: u64) -> String {
    format!("{}/s", format_bytes(bytes_per_second))
}

/// Formats a limit, where `None` means unlimited.
pub fn format_limit(limit: Option<u64>) -> String {
    match limit {
        Some(value) => format_speed(value),
        None => "unlimited".to_owned(),
    }
}

/// Formats a completion fraction as a percentage.
pub fn format_progress(progress: f32) -> String {
    if !progress.is_finite() {
        return "  ?.?%".to_owned();
    }

    format!("{:>5.1}%", progress.clamp(0.0, 1.0) * 100.0)
}

/// Formats an ETA in seconds as a short duration.
pub fn format_eta(eta_secs: Option<u64>) -> String {
    let Some(secs) = eta_secs else {
        return "-".to_owned();
    };

    let hours = secs / 3600;
    let minutes = (secs % 3600) / 60;
    let seconds = secs % 60;

    if hours > 0 {
        format!("{hours}h{minutes:02}m")
    } else if minutes > 0 {
        format!("{minutes}m{seconds:02}s")
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bt_client::{GlobalStats, TorrentId};
    use ratatui::{Terminal, backend::TestBackend};

    fn summary(name: &str) -> TorrentSummary {
        TorrentSummary {
            id: TorrentId::new(name),
            name: name.to_owned(),
            state: TorrentState::Downloading,
            progress: 0.5,
            total_bytes: 1024 * 1024,
            downloaded_bytes: 512 * 1024,
            uploaded_bytes: 0,
            download_bps: 2048,
            upload_bps: 0,
            peers_connected: 3,
            eta_secs: Some(65),
        }
    }

    fn render(state: &AppState, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| draw(frame, state))
            .expect("draw frame");

        let buffer = terminal.backend().buffer();
        let mut output = String::new();

        for y in 0..buffer.area.height {
            let mut line = String::new();
            for x in 0..buffer.area.width {
                if let Some(cell) = buffer.cell((x, y)) {
                    line.push_str(cell.symbol());
                }
            }
            output.push_str(line.trim_end());
            output.push('\n');
        }

        output
    }

    #[test]
    fn empty_state_shows_hint() {
        let output = render(&AppState::default(), 80, 24);
        assert!(output.contains("No torrents yet."), "output: {output}");
        assert!(output.contains("Torrents"), "output: {output}");
        assert!(output.contains("q quit"), "output: {output}");
    }

    #[test]
    fn table_lists_torrents() {
        let state = AppState {
            torrents: vec![summary("Example")],
            stats: GlobalStats {
                download_bps: 2048,
                upload_bps: 0,
                torrents_total: 1,
                torrents_active: 1,
                peers_connected: 3,
                downloaded_bytes: 0,
                uploaded_bytes: 0,
            },
            ..AppState::default()
        };

        let output = render(&state, 100, 24);

        assert!(output.contains("Example"), "output: {output}");
        assert!(output.contains("Downloading"), "output: {output}");
        assert!(output.contains("50.0%"), "output: {output}");
        assert!(output.contains("1.0 MiB"), "output: {output}");
        assert!(output.contains("1m05s"), "output: {output}");
        assert!(output.contains("2.0 KiB/s"), "output: {output}");
        assert!(output.contains("torrents 1 (1)"), "output: {output}");
    }

    #[test]
    fn help_overlay_shows_bindings() {
        let state = AppState {
            show_help: true,
            ..AppState::default()
        };

        let output = render(&state, 80, 24);

        assert!(output.contains("Key bindings"), "output: {output}");
        assert!(output.contains("Pause or resume"), "output: {output}");
        assert!(output.contains("Close help"), "output: {output}");
    }

    #[test]
    fn tiny_terminal_does_not_panic() {
        let state = AppState {
            torrents: vec![summary("Example")],
            show_help: true,
            ..AppState::default()
        };

        let output = render(&state, 20, 5);
        assert!(!output.is_empty());
    }

    #[test]
    fn formats_bytes() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.0 KiB");
        assert_eq!(format_bytes(1536), "1.5 KiB");
        assert_eq!(format_bytes(1024 * 1024), "1.0 MiB");
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.0 GiB");
    }

    #[test]
    fn formats_limits() {
        assert_eq!(format_limit(None), "unlimited");
        assert_eq!(format_limit(Some(0)), "0 B/s");
        assert_eq!(format_limit(Some(1024)), "1.0 KiB/s");
    }

    #[test]
    fn formats_progress_and_eta() {
        assert_eq!(format_progress(0.0), "  0.0%");
        assert_eq!(format_progress(0.5), " 50.0%");
        assert_eq!(format_progress(1.5), "100.0%");
        assert_eq!(format_progress(f32::NAN), "  ?.?%");

        assert_eq!(format_eta(None), "-");
        assert_eq!(format_eta(Some(5)), "5s");
        assert_eq!(format_eta(Some(65)), "1m05s");
        assert_eq!(format_eta(Some(3725)), "1h02m");
    }
}
