use anyhow::{Context, Result};
use clap::Parser;
use crossterm::{
    event::{self, EventStream, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use futures_util::StreamExt;
use std::collections::BTreeMap;
use tui::{
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    widgets::{Block, Borders, Cell, Row, Table, TableState},
    Frame, Terminal,
};

use rust_decimal::{prelude::FromPrimitive, Decimal};
use tastytrade_rs::{
    api::{order::Symbol, position::QuantityDirection, quote_streaming::DxFeedSymbol},
    dxfeed::{self, Event, EventData},
    TastyTrade,
};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// tastytrade username or email
    #[arg(short, long)]
    login: String,

    /// tastytrade password
    #[arg(short, long)]
    password: String,
}

#[derive(Debug)]
struct SimpleGreeks {
    theta: f64,
    delta: f64,
}

#[derive(Debug)]
struct PriceRecord {
    symbol: Symbol,
    open: Decimal,
    current: Decimal,
    amount: Decimal,
    multiplier: Decimal,
    direction: QuantityDirection,
    greeks: SimpleGreeks,
}

struct App {
    state: TableState,
    records: BTreeMap<Symbol, BTreeMap<DxFeedSymbol, PriceRecord>>,
    num_lines: usize,
}

impl App {
    fn new(records: BTreeMap<Symbol, BTreeMap<DxFeedSymbol, PriceRecord>>) -> Self {
        let num_lines = records
            .iter()
            .map(|e| e.1.len())
            .fold(records.len(), |acc, v| acc + v);

        Self {
            state: TableState::default(),
            records,
            num_lines,
        }
    }
    pub fn next(&mut self) {
        let i = match self.state.selected() {
            Some(i) => {
                if i >= self.num_lines - 1 {
                    0
                } else {
                    i + 1
                }
            }
            None => 0,
        };
        self.state.select(Some(i));
    }

    pub fn previous(&mut self) {
        let i = match self.state.selected() {
            Some(i) => {
                if i == 0 {
                    self.num_lines - 1
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        self.state.select(Some(i));
    }

    pub fn get_record(&mut self, symbol: DxFeedSymbol) -> Option<&mut PriceRecord> {
        for (_, positions) in self.records.iter_mut() {
            for (pos_symbol, pos) in positions.iter_mut() {
                if *pos_symbol == symbol {
                    return Some(pos);
                }
            }
        }
        None
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    println!("Logging in...");

    let tasty = TastyTrade::login(&args.login, &args.password, false)
        .await
        .context("Logging into tastytrade")?;

    println!("Downloading account info...");

    let mut positions = Vec::new();
    for account in tasty.accounts().await.unwrap() {
        positions.extend(account.positions().await.unwrap());
    }

    let mut stream_syms = Vec::with_capacity(positions.len());
    let mut records: BTreeMap<Symbol, BTreeMap<DxFeedSymbol, PriceRecord>> = BTreeMap::new();
    for pos in positions.iter() {
        let stream_sym = tasty
            .get_streamer_symbol(&pos.instrument_type, &pos.symbol)
            .await?;
        stream_syms.push(stream_sym.clone());

        let record = PriceRecord {
            symbol: pos.symbol.clone(),
            open: pos.average_open_price.round_dp(2),
            current: pos.close_price.round_dp(2),
            amount: pos.quantity,
            multiplier: pos.multiplier,
            direction: pos.quantity_direction,
            greeks: SimpleGreeks {
                theta: 0.0,
                delta: 0.0,
            },
        };
        records
            .entry(pos.underlying_symbol.clone())
            .or_default()
            .insert(stream_sym, record);
    }

    let mut quote_streamer = tasty.create_quote_streamer().await?;
    let quote_sub = quote_streamer.create_sub(dxfeed::DXF_ET_QUOTE | dxfeed::DXF_ET_GREEKS);
    quote_sub.add_symbols(&stream_syms);

    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(records);
    let mut keyboard_event_stream = EventStream::new();

    loop {
        tokio::select! {
            ev = quote_sub.get_event() => {
                if let Ok(Event { sym, data }) = ev {
                    if let Some(record) = app.get_record(DxFeedSymbol(sym)) {
                        match data {
                            EventData::Quote(quote) => {
                                record.current = Decimal::from_f64((quote.bid_price + quote.ask_price) / 2.0).unwrap();
                            }
                            EventData::Greeks(greeks) => {
                                record.greeks = SimpleGreeks {
                                    theta: greeks.theta,
                                    delta: greeks.delta,
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            maybe_event = keyboard_event_stream.next() => {
                match maybe_event {
                    Some(Ok(event)) => {
                        if let event::Event::Key(key) = event {
                            if key.kind == KeyEventKind::Press {
                                match key.code {
                                    KeyCode::Char('q') => break,
                                    KeyCode::Down => app.next(),
                                    KeyCode::Up => app.previous(),
                                    _ => {}
                                }
                            }

                        }
                    }
                    Some(Err(e)) => println!("Error: {:?}\r", e),
                    None => break,
                }
            }
        }

        terminal.draw(|f| ui(f, &mut app))?;
    }

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen,)?;
    terminal.show_cursor()?;

    Ok(())
}

fn ui<B: Backend>(f: &mut Frame<B>, app: &mut App) {
    let rects = Layout::default()
        .constraints([Constraint::Percentage(100)].as_ref())
        .margin(2)
        .split(f.size());

    let selected_style = Style::default().add_modifier(Modifier::REVERSED);
    let normal_style = Style::default().bg(Color::Blue);
    let header_cells = [
        "SYMBOL",
        "CURRENT",
        "AMOUNT",
        "TRADE PRICE",
        "PROFIT",
        "THETA",
        "DELTA",
    ]
    .iter()
    .map(|h| Cell::from(*h).style(Style::default().fg(Color::Red)));
    let header = Row::new(header_cells).style(normal_style).height(1);

    let rows = app.records.iter().flat_map(|(underlying_symbol, records)| {
        let mut rows = vec![Row::new(vec![underlying_symbol.0.clone()]).height(1)];
        for rec in records.values() {
            let to_net = |value: Decimal| -> Decimal {
                (value
                    * rec.amount
                    * rec.multiplier
                    * if let QuantityDirection::Short = rec.direction {
                        Decimal::from(-1)
                    } else {
                        Decimal::from(1)
                    })
                .round_dp(2)
            };
            let profit = to_net(rec.current - rec.open);
            let theta = to_net(Decimal::from_f64(rec.greeks.theta).unwrap());
            let delta = to_net(Decimal::from_f64(rec.greeks.delta).unwrap());

            let name = if rec.symbol == *underlying_symbol {
                "SHARES".to_owned()
            } else {
                rec.symbol.0.clone()
            };
            let cells = vec![
                format!(" {}", name),
                rec.current.round_dp(2).to_string(),
                rec.amount.round_dp(5).to_string(),
                rec.open.to_string(),
                profit.to_string(),
                theta.to_string(),
                delta.to_string(),
            ];
            rows.push(Row::new(cells).height(1))
        }
        rows
    });

    let t = Table::new(rows)
        .header(header)
        .block(Block::default().borders(Borders::ALL))
        .highlight_style(selected_style)
        .highlight_symbol(">> ")
        .widths(&[
            Constraint::Length(25),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(12),
        ]);

    f.render_stateful_widget(t, rects[0], &mut app.state);
}
