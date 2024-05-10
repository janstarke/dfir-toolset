use num_traits::cast::AsPrimitive;
use std::collections::{BTreeSet, HashSet};
use std::fmt::Display;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::{fs::File, path::Path};

use dfir_toolkit::common::FormattableDatetime;
use evtx::{EvtxParser, SerializedEvtxRecord};
use ouroboros::self_referencing;
use ratatui::layout::{Constraint, Rect};
use ratatui::widgets::{Block, BorderType, HighlightSpacing, TableState};
use ratatui::Frame;
use ratatui::{
    style::{Modifier, Style},
    text::Text,
    widgets::{Cell, Row, Table},
};

use super::color_scheme::{ColorScheme, PALETTES};
use super::RowContents;

#[derive(Eq, PartialEq, Hash)]
pub enum EventFilter {
    ExcludeByEventId(u32),
    IncludeByEventId(u32),
}

impl EventFilter {
    pub fn filter(&self, rc: &RowContents) -> bool {
        match self {
            EventFilter::ExcludeByEventId(event_id) => rc.event().system().EventID() != event_id,
            EventFilter::IncludeByEventId(event_id) => rc.event().system().EventID() == event_id,
        }
    }
}

#[derive(Default, Copy, Clone)]
pub enum ReadState {
    #[default]
    Preparing,
    Running(f32),
    Finished,
}

impl Display for ReadState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadState::Preparing => write!(f, "preparing"),
            ReadState::Running(s) => write!(f, "{:.2}%", s * 100.0),
            ReadState::Finished => write!(f, ""),
        }
    }
}

#[derive(Default)]
struct EvtxTableData {
    rows: BTreeSet<RowContents>,
    sparkline_data: Vec<u64>,
    number_of_records: usize,
    state: ReadState,
}

pub struct EvtxTable {
    data: Arc<Mutex<EvtxTableData>>,
    _reader: JoinHandle<anyhow::Result<()>>,
    colors: ColorScheme,
    timestamp_width: u16,
    event_filters: HashSet<EventFilter>,
    filtered_rows_count: usize,
}

fn load_events(path: PathBuf, data: Arc<Mutex<EvtxTableData>>) -> anyhow::Result<()> {
    if let Ok(mut data) = data.lock() {
        data.number_of_records = EvtxParser::from_path(&path)?.records().count();
    }

    let mut count: usize = 0;
    for row in RowContentsIterator::try_from(path.as_path())? {
        if let Ok(mut data) = data.lock() {
            count += 1;
            if data.number_of_records > 0 {
                let c: f32 = count.as_();
                let n: f32 = data.number_of_records.as_();
                data.state = ReadState::Running(c / n);
            }

            let record_timestamp = row.record_timestamp().timestamp();
            data.rows.insert(row);

            // update sparkline data
            if let Some(first_ts) = data.rows.first() {
                if let Some(last_ts) = data.rows.last() {
                    let mut first_ts = first_ts.record_timestamp().timestamp();
                    let last_ts = last_ts.record_timestamp().timestamp();
                    assert!(last_ts >= first_ts);
                    let step_size = i64::max(1, (last_ts - first_ts) / 3600);

                    first_ts /= step_size;

                    let ts = usize::try_from((record_timestamp / step_size) - first_ts)?;
                    while ts + 1 > data.sparkline_data.len() {
                        data.sparkline_data.push(0)
                    }
                    data.sparkline_data[ts] += 1;
                }
            }
        } else {
            break;
        }
    }

    if let Ok(mut data) = data.lock() {
        data.state = ReadState::Finished;
    }
    Ok(())
}

impl TryFrom<&Path> for EvtxTable {
    type Error = anyhow::Error;

    fn try_from(path: &Path) -> Result<Self, Self::Error> {
        let data = Arc::new(Mutex::new(EvtxTableData::default()));
        let path = path.to_path_buf();
        let reader_data = Arc::clone(&data);
        let _reader = thread::spawn(move || load_events(path, reader_data));

        let timestamp_width = u16::try_from(
            FormattableDatetime::from(chrono::offset::Utc::now())
                .to_string()
                .len(),
        )?;

        Ok(EvtxTable {
            data,
            _reader,
            colors: ColorScheme::new(&PALETTES[0]),
            timestamp_width,
            event_filters: HashSet::new(),
            filtered_rows_count: 0,
        })
    }
}

impl EvtxTable {
    pub fn render(&mut self, frame: &mut Frame, area: Rect, state: &mut TableState) {
        let block = Block::bordered()
            .title(
                self.read_status()
                    .map(|s| s.to_string())
                    .unwrap_or("".to_string()),
            )
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(self.colors.footer_border_color()));

        let header_style = Style::default()
            .fg(self.colors.header_fg())
            .bg(self.colors.header_bg());

        let column_headers = ["", "Timestamp", "Record#", "Event#", "UserID", "Data"];
        let header = column_headers
            .into_iter()
            .map(Cell::from)
            .collect::<Row>()
            .style(header_style)
            .height(1);

        let selected_style = Style::default()
            .add_modifier(Modifier::REVERSED)
            .fg(self.colors.selected_style_fg());

        let bar = " █ ";

        let mut table = Table::default()
            .widths(vec![
                Constraint::Length(2),
                Constraint::Length(self.timestamp_width),
                Constraint::Length(column_headers[1].len() as u16),
                Constraint::Length(column_headers[1].len() as u16),
                Constraint::Length(20),
                Constraint::Min(1),
            ])
            .header(header)
            .highlight_style(selected_style)
            .highlight_symbol(Text::from(vec![
                "".into(),
                bar.into(),
                bar.into(),
                bar.into(),
                bar.into(),
            ]))
            //.bg(self.colors.buffer_bg())
            .highlight_spacing(HighlightSpacing::Always);

        if let Ok(data) = self.data.lock() {
            table = table.rows(
                data.rows
                    .iter()
                    .filter(|rc| self.filter_row(rc))
                    .map(Row::from),
            );
            frame.render_stateful_widget(table.block(block), area, state);
        } else {
            panic!("unable to acquire data lock");
        }
    }

    pub fn update(&mut self) {
        if let Ok(data) = self.data.lock() {
            self.filtered_rows_count = data.rows.iter().filter(|rc| self.filter_row(rc)).count()
        }
    }

    fn filter_row(&self, rc: &RowContents) -> bool {
        self.event_filters
            .iter()
            .map(|filter| filter.filter(rc))
            .fold(true, |a, b| a & b)
    }

    pub fn len(&self) -> usize {
        self.filtered_rows_count
    }

    pub fn is_empty(&self) -> bool {
        self.filtered_rows_count == 0
    }

    pub fn content(&self, filtered_row_id: usize) -> Option<String> {
        if let Ok(data) = self.data.lock() {
            data.rows
                .iter()
                .filter(|rc| self.filter_row(rc))
                .nth(filtered_row_id)
                .map(|r| r.raw_value().clone())
        } else {
            None
        }
    }

    pub fn with_sparkline_data<F>(&self, mut f: F)
    where
        F: FnMut(&Vec<u64>),
    {
        if let Ok(data) = self.data.lock() {
            f(&data.sparkline_data)
        }
    }

    pub fn event_id_in_row(&self, filtered_row_id: usize) -> Option<u32> {
        if let Ok(data) = self.data.lock() {
            data.rows
                .iter()
                .filter(|rc| self.filter_row(rc))
                .nth(filtered_row_id)
                .map(|r| *r.event().system().EventID())
        } else {
            None
        }
    }

    pub fn exclude_event_id(&mut self, filtered_row_id: usize) {
        if let Some(event_id) = self.event_id_in_row(filtered_row_id) {
            self.event_filters
                .insert(EventFilter::ExcludeByEventId(event_id));
        }
        self.update();
    }
    pub fn include_event_id(&mut self, filtered_row_id: usize) {
        if let Some(event_id) = self.event_id_in_row(filtered_row_id) {
            self.event_filters
                .insert(EventFilter::IncludeByEventId(event_id));
        }
        self.update();
    }
    pub fn reset_filter(&mut self) {
        self.event_filters.clear();
        self.update();
    }

    pub fn read_status(&self) -> Option<ReadState> {
        self.data.lock().ok().map(|data| data.state)
    }
}

#[self_referencing]
pub struct RowContentsIterator {
    parser: EvtxParser<File>,

    #[borrows(mut parser)]
    #[covariant]
    iterator: Box<dyn Iterator<Item = evtx::err::Result<SerializedEvtxRecord<String>>> + 'this>,
}

impl TryFrom<&Path> for RowContentsIterator {
    type Error = anyhow::Error;

    fn try_from(evtx_file: &Path) -> Result<Self, Self::Error> {
        let parser = EvtxParser::from_path(evtx_file)?;
        Ok(RowContentsIteratorBuilder {
            parser,
            iterator_builder: |parser| Box::new(parser.records()),
        }
        .build())
    }
}

impl Iterator for RowContentsIterator {
    type Item = RowContents;

    fn next(&mut self) -> Option<Self::Item> {
        self.with_iterator_mut(|iterator| match iterator.next() {
            Some(Err(why)) => panic!("Error while reading record: {why}"),
            Some(Ok(r)) => match (&r).try_into() {
                Ok(contents) => Some(contents),
                Err(why) => panic!("Error while parsing record: {why}"),
            },
            None => None,
        })
    }
}
