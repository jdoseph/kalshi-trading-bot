//! Terminal dashboard — a read-only view over the execution engine.
//!
//! Split into a pure [`DashboardState`] (what to show, unit-testable with no
//! terminal) and the rendering/event loop (verified by running it).

use crate::client::RequestSender;
use crate::market_data::MarketData;
use crate::models::{Balance, Position};
use anyhow::Result;
use std::io::{self, Stdout};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Cell, Paragraph, Row, Table},
};

/// Everything the dashboard renders. Pure data — no terminal, no I/O.
#[derive(Debug, Clone, Default)]
pub struct DashboardState {
    pub balance_usd: f64,
    pub positions: Vec<Position>,
    /// True when live trading is permitted (both gates open).
    pub live_trading: bool,
    /// True when the circuit breaker is currently tripped.
    pub breaker_tripped: bool,
    /// Human-readable time of the last successful refresh.
    pub last_refresh: String,
    /// Last error message, if the most recent refresh failed.
    pub last_error: Option<String>,
}

impl DashboardState {
    /// Build the display state from a fresh balance + positions snapshot.
    pub fn from_snapshot(balance: Balance, positions: Vec<Position>, now: &str) -> Self {
        Self {
            balance_usd: balance.usd(),
            positions,
            live_trading: false,
            breaker_tripped: false,
            last_refresh: now.to_string(),
            last_error: None,
        }
    }

    /// The mode label shown in the header.
    pub fn mode_label(&self) -> &'static str {
        if self.live_trading {
            "LIVE"
        } else {
            "DRY-RUN"
        }
    }

    pub fn position_count(&self) -> usize {
        self.positions.len()
    }
}

/// RAII guard that puts the terminal into raw/alt-screen mode and restores it
/// on drop — so a panic or early return never leaves the user's shell wrecked.
struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalGuard {
    fn new() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let terminal = Terminal::new(CrosstermBackend::new(stdout))?;
        Ok(Self { terminal })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

/// Run the dashboard. `refresh` produces a fresh snapshot; the loop polls it on
/// an interval and on demand ('r'), and quits on 'q'.
pub fn run<S, F>(live_trading: bool, mut refresh: F) -> Result<()>
where
    S: RequestSender,
    F: FnMut() -> DashboardState,
{
    let mut guard = TerminalGuard::new()?;

    let state = Arc::new(Mutex::new({
        let mut s = refresh();
        s.live_trading = live_trading;
        s
    }));

    let refresh_every = Duration::from_secs(5);
    let mut last_poll = Instant::now();

    loop {
        {
            let s = state.lock().unwrap();
            guard.terminal.draw(|f| draw(f, &s))?;
        }

        // Handle input with a short timeout so the refresh timer stays responsive.
        if event::poll(Duration::from_millis(200))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        KeyCode::Char('r') => {
                            let mut s = refresh();
                            s.live_trading = live_trading;
                            *state.lock().unwrap() = s;
                            last_poll = Instant::now();
                        }
                        _ => {}
                    }
                }
            }
        }

        if last_poll.elapsed() >= refresh_every {
            let mut s = refresh();
            s.live_trading = live_trading;
            *state.lock().unwrap() = s;
            last_poll = Instant::now();
        }
    }

    Ok(())
}

/// One-shot refresh helper: pull balance + positions via the engine, formatting
/// any error into the state's `last_error` rather than propagating.
pub fn refresh_snapshot<S: RequestSender>(md: &MarketData<S>, now: &str) -> DashboardState {
    match (md.balance(), md.positions()) {
        (Ok(b), Ok(p)) => DashboardState::from_snapshot(b, p, now),
        (Err(e), _) | (_, Err(e)) => DashboardState {
            last_error: Some(format!("{e}")),
            last_refresh: now.to_string(),
            ..Default::default()
        },
    }
}

fn draw(f: &mut Frame, s: &DashboardState) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // header
            Constraint::Min(5),    // positions
            Constraint::Length(4), // status
        ])
        .split(f.area());

    // Header: balance + mode badge.
    let mode = s.mode_label();
    let mode_color = if s.live_trading { Color::Red } else { Color::Green };
    let header = Paragraph::new(Line::from(vec![
        Span::raw(format!("  Balance: ${:.2}    Positions: {}    ", s.balance_usd, s.position_count())),
        Span::styled(format!(" {mode} "), Style::default().fg(Color::Black).bg(mode_color).add_modifier(Modifier::BOLD)),
    ]))
    .block(Block::default().borders(Borders::ALL).title(" Kalshi Execution Engine "));
    f.render_widget(header, chunks[0]);

    // Positions table.
    let rows: Vec<Row> = s
        .positions
        .iter()
        .map(|p| {
            Row::new(vec![
                Cell::from(p.ticker.clone()),
                Cell::from(format!("{}", p.shares)),
                Cell::from(format!("{:.2}", p.exposure_usd)),
            ])
        })
        .collect();
    let table = Table::new(
        rows,
        [Constraint::Percentage(60), Constraint::Percentage(20), Constraint::Percentage(20)],
    )
    .header(Row::new(vec!["Ticker", "Position", "Exposure $"]).style(Style::default().add_modifier(Modifier::BOLD)))
    .block(Block::default().borders(Borders::ALL).title(" Positions "));
    f.render_widget(table, chunks[1]);

    // Status line.
    let breaker = if s.breaker_tripped {
        Span::styled("TRIPPED", Style::default().fg(Color::Red).add_modifier(Modifier::BOLD))
    } else {
        Span::styled("OK", Style::default().fg(Color::Green))
    };
    let mut lines = vec![Line::from(vec![
        Span::raw(" Circuit breaker: "),
        breaker,
        Span::raw(format!("     Last refresh: {}", s.last_refresh)),
    ])];
    if let Some(err) = &s.last_error {
        lines.push(Line::from(Span::styled(format!(" ! {err}"), Style::default().fg(Color::Red))));
    }
    lines.push(Line::from(Span::styled(" [q] quit   [r] refresh now", Style::default().fg(Color::DarkGray))));
    let status = Paragraph::new(lines).block(Block::default().borders(Borders::ALL));
    f.render_widget(status, chunks[2]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pos(ticker: &str, shares: i64, exposure_cents: i64) -> Position {
        // Build via the real Kalshi wire shape (dollar strings).
        serde_json::from_value(serde_json::json!({
            "ticker": ticker,
            "position_fp": format!("{shares}.00"),
            "total_traded_dollars": format!("{:.2}", exposure_cents as f64 / 100.0),
            "market_exposure_dollars": format!("{:.2}", exposure_cents as f64 / 100.0),
            "fees_paid_dollars": "0.00",
        }))
        .unwrap()
    }

    #[test]
    fn summarizes_balance_and_positions() {
        let s = DashboardState::from_snapshot(
            Balance { balance: 4200 },
            vec![pos("A", 5, 250), pos("B", -3, 100)],
            "13:00:00",
        );
        assert_eq!(s.balance_usd, 42.0);
        assert_eq!(s.position_count(), 2);
        assert_eq!(s.last_refresh, "13:00:00");
        assert!(s.last_error.is_none());
    }

    #[test]
    fn mode_label_reflects_trading_gate() {
        let mut s = DashboardState::default();
        assert_eq!(s.mode_label(), "DRY-RUN");
        s.live_trading = true;
        assert_eq!(s.mode_label(), "LIVE");
    }

    /// Render to an in-memory backend and assert the key facts appear on screen.
    /// This exercises the real `draw` path without a terminal.
    #[test]
    fn draw_renders_balance_positions_and_mode() {
        use ratatui::backend::TestBackend;

        let state = DashboardState::from_snapshot(
            Balance { balance: 12345 },
            vec![pos("KXTEST-YES", 7, 500)],
            "13:45:01",
        );

        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &state)).unwrap();

        let buf = terminal.backend().buffer().clone();
        let screen: String = buf.content().iter().map(|c| c.symbol()).collect();

        assert!(screen.contains("$123.45"), "balance missing:\n{screen}");
        assert!(screen.contains("KXTEST-YES"), "position row missing");
        assert!(screen.contains("DRY-RUN"), "mode badge missing");
        assert!(screen.contains("Circuit breaker"), "status line missing");
        assert!(screen.contains("13:45:01"), "refresh time missing");
    }

    #[test]
    fn draw_shows_error_when_refresh_failed() {
        use ratatui::backend::TestBackend;
        let state = DashboardState {
            last_error: Some("HTTP 401: unauthorized".into()),
            last_refresh: "13:00:00".into(),
            ..Default::default()
        };
        let backend = TestBackend::new(80, 20);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal.draw(|f| draw(f, &state)).unwrap();
        let buf = terminal.backend().buffer().clone();
        let screen: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(screen.contains("401"), "error not surfaced:\n{screen}");
    }
}
