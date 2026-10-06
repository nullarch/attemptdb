//! Query results and their renderings (JSON, table, CSV).

use crate::Result;
use crate::ids::{hyphenated, prefix_for_column};
use attemptdb_core::Timestamp;
use comfy_table::{ContentArrangement, Table, presets};
use datafusion::arrow::array::{Array, ArrayRef, AsArray, RecordBatch};
use datafusion::arrow::compute::cast;
use datafusion::arrow::datatypes::{
    DataType, Float32Type, Float64Type, Int8Type, Int16Type, Int32Type, Int64Type, SchemaRef,
    TimeUnit, TimestampMicrosecondType, TimestampMillisecondType, TimestampNanosecondType,
    TimestampSecondType, UInt8Type, UInt16Type, UInt32Type, UInt64Type,
};
use datafusion::arrow::util::display::{ArrayFormatter, FormatOptions};
use serde_json::{Map, Number, Value};
use std::cell::Cell;
use std::sync::Arc;

/// Maximum characters per rendered table cell.
pub const CELL_LIMIT: usize = 80;

/// Narrowest column (characters, borders included) `render_table` will wrap
/// down to before it gives up on the width limit.
pub const MIN_COLUMN_WIDTH: usize = 10;

/// What a result represents.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResultKind {
    /// Ordinary rows.
    Rows,
    /// A `WHY` / `TRACE` / `EXPLAIN` answer: rows that explain something.
    Explanation,
    /// The question was understood but nothing matched; see `notes`.
    Empty,
}

/// Rows plus the notes (uncertainty, evidence remarks) that accompany them.
#[derive(Clone, Debug)]
pub struct QueryResult {
    pub schema: SchemaRef,
    pub batches: Vec<RecordBatch>,
    pub kind: ResultKind,
    pub notes: Vec<String>,
    /// Set by the limited execution paths ([`crate::QueryLimits`]): the
    /// statement had more rows than the limit allowed, and the rest were
    /// never produced. `row_count` is then the limit, not the total.
    pub truncated: bool,
}

impl QueryResult {
    /// The rows as an Arrow IPC stream (schema first), for carrying a
    /// result between processes; [`Self::from_ipc_bytes`] reads it back.
    pub fn to_ipc_bytes(&self) -> Result<Vec<u8>> {
        use datafusion::arrow::ipc::writer::StreamWriter;
        let mut buf = Vec::new();
        {
            let mut w = StreamWriter::try_new(&mut buf, &self.schema)?;
            for b in &self.batches {
                w.write(b)?;
            }
            w.finish()?;
        }
        Ok(buf)
    }

    /// A result from [`Self::to_ipc_bytes`] plus the kind and notes that
    /// do not travel in the stream.
    pub fn from_ipc_bytes(bytes: &[u8], kind: ResultKind, notes: Vec<String>) -> Result<Self> {
        use datafusion::arrow::ipc::reader::StreamReader;
        let reader = StreamReader::try_new(std::io::Cursor::new(bytes), None)?;
        let schema = reader.schema();
        let batches = reader.collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(Self::new(schema, batches, kind, notes))
    }

    pub fn new(
        schema: SchemaRef,
        batches: Vec<RecordBatch>,
        kind: ResultKind,
        notes: Vec<String>,
    ) -> Self {
        Self {
            schema,
            batches,
            kind,
            notes,
            truncated: false,
        }
    }

    /// An empty result carrying the schema the rows would have had.
    pub fn empty(schema: SchemaRef, note: impl Into<String>) -> Self {
        Self {
            schema,
            batches: Vec::new(),
            kind: ResultKind::Empty,
            notes: vec![note.into()],
            truncated: false,
        }
    }

    /// The first `limit` rows (zero-copy slices). `truncated` is set when
    /// rows were dropped or the result already was truncated.
    pub fn take_rows(&self, limit: usize) -> QueryResult {
        let total = self.row_count();
        if total <= limit {
            return self.clone();
        }
        let mut batches = Vec::new();
        let mut remaining = limit;
        for b in &self.batches {
            if remaining == 0 {
                break;
            }
            let n = b.num_rows().min(remaining);
            batches.push(b.slice(0, n));
            remaining -= n;
        }
        let mut out = QueryResult::new(
            Arc::clone(&self.schema),
            batches,
            self.kind,
            self.notes.clone(),
        );
        out.truncated = true;
        out
    }

    pub fn row_count(&self) -> usize {
        self.batches.iter().map(RecordBatch::num_rows).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.row_count() == 0
    }

    pub fn column_names(&self) -> Vec<String> {
        self.schema
            .fields()
            .iter()
            .map(|f| f.name().clone())
            .collect()
    }

    /// Index of a column by name.
    pub fn column(&self, name: &str) -> Option<usize> {
        self.schema.index_of(name).ok()
    }

    /// Every row as JSON objects keyed by column name. Binary ids render as
    /// prefixed text, timestamps as RFC 3339, lists as arrays.
    pub fn to_json(&self) -> Value {
        self.capped(usize::MAX, usize::MAX, usize::MAX).json_array()
    }

    /// Every row as text cells (same conversions as [`Self::to_json`], with
    /// nulls as empty strings and lists joined by `, `).
    pub fn cells(&self) -> Vec<Vec<String>> {
        self.capped(usize::MAX, usize::MAX, usize::MAX).cells()
    }

    /// The leading rows that fit `max_rows` and a byte budget, converted to
    /// JSON values one row at a time: nothing past the budget is ever
    /// converted, and a cell longer than `max_cell_bytes` is cut (and
    /// counted) while it is read, so a result with megabyte cells costs the
    /// budget, not the megabytes. The budget is measured as the compact JSON
    /// size of the kept rows (keys included); a first row that alone exceeds
    /// it is not kept.
    pub fn capped(&self, max_rows: usize, max_bytes: usize, max_cell_bytes: usize) -> CappedRows {
        let names = self.column_names();
        let name_bytes: usize = names.iter().map(|n| n.len() + 4).sum();
        let opts = CellOpts {
            max_bytes: max_cell_bytes,
            clipped: Cell::new(0),
        };
        let mut rows: Vec<Vec<Value>> = Vec::new();
        let mut bytes = 0usize;
        let total = self.row_count();
        let mut stopped_by = None;
        'batches: for batch in &self.batches {
            let cols = decoded_columns(batch);
            for row in 0..batch.num_rows() {
                if rows.len() >= max_rows {
                    stopped_by = Some(CapReason::Rows);
                    break 'batches;
                }
                let clipped_before = opts.clipped.get();
                let cells: Vec<Value> = names
                    .iter()
                    .enumerate()
                    .map(|(i, name)| cell_json_with(cols[i].as_ref(), row, name, &opts))
                    .collect();
                let size = 2 + name_bytes + cells.iter().map(json_len).sum::<usize>();
                if bytes.saturating_add(size) > max_bytes {
                    // The row is not kept, so neither are its clipped cells.
                    opts.clipped.set(clipped_before);
                    stopped_by = Some(CapReason::Bytes);
                    break 'batches;
                }
                bytes += size;
                rows.push(cells);
            }
        }
        CappedRows {
            columns: names,
            omitted_rows: total - rows.len(),
            rows,
            bytes,
            stopped_by,
            clipped_cells: opts.clipped.get(),
        }
    }

    /// A comfy-table rendering with a `(n rows)` footer; long cells are
    /// truncated at 80 characters. `max_width` enables dynamic column
    /// wrapping to that many characters. `notes` are not included: callers
    /// print them after the table.
    pub fn render_table(&self, max_width: Option<usize>) -> String {
        self.capped(usize::MAX, usize::MAX, usize::MAX)
            .render_table(max_width)
    }

    /// RFC 4180-style CSV with a header row. A text cell that a spreadsheet
    /// would read as a formula (it starts with `=`, `+`, `-`, `@`, TAB or
    /// CR) is prefixed with a single quote; numbers are left alone.
    pub fn render_csv(&self) -> String {
        self.capped(usize::MAX, usize::MAX, usize::MAX).render_csv()
    }
}

/// Why [`QueryResult::capped`] stopped before the end of the result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapReason {
    /// The row cap.
    Rows,
    /// The byte budget.
    Bytes,
}

/// The leading rows of a [`QueryResult`] as JSON values, see
/// [`QueryResult::capped`].
#[derive(Clone, Debug)]
pub struct CappedRows {
    pub columns: Vec<String>,
    /// One value per column, in column order, for each kept row.
    pub rows: Vec<Vec<Value>>,
    /// Compact JSON size of the kept rows.
    pub bytes: usize,
    /// Rows of the source result that were not kept.
    pub omitted_rows: usize,
    pub stopped_by: Option<CapReason>,
    /// Cells of kept rows that were cut at the per-cell limit.
    pub clipped_cells: usize,
}

impl CappedRows {
    pub fn returned(&self) -> usize {
        self.rows.len()
    }

    /// The kept rows as JSON objects keyed by column name.
    pub fn json_array(&self) -> Value {
        Value::Array(
            self.rows
                .iter()
                .map(|cells| {
                    let mut obj = Map::with_capacity(self.columns.len());
                    for (name, v) in self.columns.iter().zip(cells) {
                        obj.insert(name.clone(), v.clone());
                    }
                    Value::Object(obj)
                })
                .collect(),
        )
    }

    /// The kept rows as text cells (nulls empty, lists joined by `, `).
    pub fn cells(&self) -> Vec<Vec<String>> {
        self.rows
            .iter()
            .map(|cells| cells.iter().map(value_text).collect())
            .collect()
    }

    /// Table rendering of the kept rows; the footer counts them.
    pub fn render_table(&self, max_width: Option<usize>) -> String {
        let mut table = Table::new();
        table.load_preset(presets::UTF8_FULL_CONDENSED);
        // Wrap to `max_width` only while every column keeps a readable
        // minimum; otherwise let the table run wide rather than squeezing
        // twenty columns into four characters each.
        let columns = self.columns.len().max(1);
        if let Some(w) = max_width
            && w / columns >= MIN_COLUMN_WIDTH
        {
            table.set_content_arrangement(ContentArrangement::Dynamic);
            table.set_width(w.clamp(20, usize::from(u16::MAX)) as u16);
        }
        table.set_header(self.columns.clone());
        for row in self.cells() {
            table.add_row(row.iter().map(|c| truncate(c, CELL_LIMIT)));
        }
        let n = self.rows.len();
        let mut out = String::new();
        if !self.columns.is_empty() {
            out.push_str(&table.to_string());
            out.push('\n');
        }
        out.push_str(&format!("({n} row{})", if n == 1 { "" } else { "s" }));
        out
    }

    /// CSV of the kept rows with formula-injection neutralisation.
    pub fn render_csv(&self) -> String {
        let mut out = String::new();
        out.push_str(
            &self
                .columns
                .iter()
                .map(|c| csv_escape(&neutralise_formula(c)))
                .collect::<Vec<_>>()
                .join(","),
        );
        out.push('\n');
        for row in &self.rows {
            out.push_str(&row.iter().map(csv_cell).collect::<Vec<_>>().join(","));
            out.push('\n');
        }
        out
    }
}

/// Truncate to `limit` characters, marking the cut with an ellipsis.
pub fn truncate(s: &str, limit: usize) -> String {
    if s.chars().count() <= limit {
        return s.to_string();
    }
    let mut out: String = s.chars().take(limit.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn csv_escape(s: &str) -> String {
    if s.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

/// Prefix a single quote when a spreadsheet would read `s` as a formula
/// (OWASP CSV injection: a leading `=`, `+`, `-`, `@`, TAB or CR).
fn neutralise_formula(s: &str) -> String {
    match s.chars().next() {
        Some('=' | '+' | '-' | '@' | '\t' | '\r') => format!("'{s}"),
        _ => s.to_string(),
    }
}

/// One CSV cell: text is neutralised, numbers and booleans are written as
/// they are (a negative number must stay a number).
fn csv_cell(v: &Value) -> String {
    match v {
        Value::Number(_) | Value::Bool(_) | Value::Null => csv_escape(&value_text(v)),
        other => csv_escape(&neutralise_formula(&value_text(other))),
    }
}

/// The compact-JSON size of a value (an estimate that never undercounts
/// by more than the escapes of exotic control characters).
fn json_len(v: &Value) -> usize {
    match v {
        Value::Null => 4,
        Value::Bool(b) => {
            if *b {
                4
            } else {
                5
            }
        }
        Value::Number(n) => n.to_string().len(),
        Value::String(s) => {
            2 + s.len()
                + s.bytes()
                    .filter(|b| matches!(b, b'"' | b'\\') || *b < 0x20)
                    .count()
        }
        Value::Array(a) => 2 + a.iter().map(|x| json_len(x) + 1).sum::<usize>(),
        Value::Object(o) => {
            2 + o
                .iter()
                .map(|(k, x)| k.len() + 4 + json_len(x))
                .sum::<usize>()
        }
    }
}

/// How a cell is converted: strings longer than `max_bytes` are cut at a
/// character boundary and counted in `clipped`.
struct CellOpts {
    max_bytes: usize,
    clipped: Cell<usize>,
}

impl CellOpts {
    fn string(&self, s: &str) -> Value {
        if s.len() <= self.max_bytes {
            return Value::String(s.to_string());
        }
        let mut cut = self.max_bytes;
        while cut > 0 && !s.is_char_boundary(cut) {
            cut -= 1;
        }
        self.clipped.set(self.clipped.get() + 1);
        Value::String(format!(
            "{}… [cut: {} more bytes]",
            &s[..cut],
            s.len() - cut
        ))
    }
}

/// Columns with dictionaries decoded so per-cell access is uniform.
fn decoded_columns(batch: &RecordBatch) -> Vec<ArrayRef> {
    batch
        .columns()
        .iter()
        .map(|c| match c.data_type() {
            DataType::Dictionary(_, value) => cast(c, value).unwrap_or_else(|_| c.clone()),
            _ => c.clone(),
        })
        .collect()
}

fn f32_short(v: f32) -> Value {
    // `f32::to_string` yields the shortest round-tripping form ("0.9"),
    // which is what people expect to see for a confidence.
    v.to_string()
        .parse::<f64>()
        .ok()
        .and_then(Number::from_f64)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

fn f64_value(v: f64) -> Value {
    Number::from_f64(v)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn ts_value(micros: i64) -> Value {
    Value::String(Timestamp::from_micros(micros).to_rfc3339())
}

/// One cell as JSON. `name` selects the id prefix for binary id columns.
fn cell_json_with(arr: &dyn Array, row: usize, name: &str, opts: &CellOpts) -> Value {
    if row >= arr.len() || arr.is_null(row) {
        return Value::Null;
    }
    match arr.data_type() {
        DataType::Null => Value::Null,
        DataType::Utf8 => opts.string(arr.as_string::<i32>().value(row)),
        DataType::LargeUtf8 => opts.string(arr.as_string::<i64>().value(row)),
        DataType::Utf8View => opts.string(arr.as_string_view().value(row)),
        DataType::Boolean => Value::Bool(arr.as_boolean().value(row)),
        DataType::Int8 => Value::from(arr.as_primitive::<Int8Type>().value(row)),
        DataType::Int16 => Value::from(arr.as_primitive::<Int16Type>().value(row)),
        DataType::Int32 => Value::from(arr.as_primitive::<Int32Type>().value(row)),
        DataType::Int64 => Value::from(arr.as_primitive::<Int64Type>().value(row)),
        DataType::UInt8 => Value::from(arr.as_primitive::<UInt8Type>().value(row)),
        DataType::UInt16 => Value::from(arr.as_primitive::<UInt16Type>().value(row)),
        DataType::UInt32 => Value::from(arr.as_primitive::<UInt32Type>().value(row)),
        DataType::UInt64 => Value::from(arr.as_primitive::<UInt64Type>().value(row)),
        DataType::Float32 => f32_short(arr.as_primitive::<Float32Type>().value(row)),
        DataType::Float64 => f64_value(arr.as_primitive::<Float64Type>().value(row)),
        DataType::Timestamp(unit, _) => match unit {
            TimeUnit::Second => ts_value(
                arr.as_primitive::<TimestampSecondType>()
                    .value(row)
                    .saturating_mul(1_000_000),
            ),
            TimeUnit::Millisecond => ts_value(
                arr.as_primitive::<TimestampMillisecondType>()
                    .value(row)
                    .saturating_mul(1_000),
            ),
            TimeUnit::Microsecond => {
                ts_value(arr.as_primitive::<TimestampMicrosecondType>().value(row))
            }
            TimeUnit::Nanosecond => ts_value(
                arr.as_primitive::<TimestampNanosecondType>()
                    .value(row)
                    .div_euclid(1_000),
            ),
        },
        DataType::FixedSizeBinary(16) => {
            let v = arr.as_fixed_size_binary().value(row);
            let mut bytes = [0u8; 16];
            bytes.copy_from_slice(v);
            Value::String(format!("{}{}", prefix_for_column(name), hyphenated(bytes)))
        }
        DataType::FixedSizeBinary(_) => Value::String(hex(arr.as_fixed_size_binary().value(row))),
        DataType::Binary => Value::String(hex(arr.as_binary::<i32>().value(row))),
        DataType::LargeBinary => Value::String(hex(arr.as_binary::<i64>().value(row))),
        DataType::List(_) => {
            let inner = arr.as_list::<i32>().value(row);
            Value::Array(
                (0..inner.len())
                    .map(|i| cell_json_with(inner.as_ref(), i, name, opts))
                    .collect(),
            )
        }
        DataType::LargeList(_) => {
            let inner = arr.as_list::<i64>().value(row);
            Value::Array(
                (0..inner.len())
                    .map(|i| cell_json_with(inner.as_ref(), i, name, opts))
                    .collect(),
            )
        }
        DataType::Struct(fields) => {
            let s = arr.as_struct();
            let mut obj = Map::new();
            for (i, f) in fields.iter().enumerate() {
                obj.insert(
                    f.name().clone(),
                    cell_json_with(s.column(i).as_ref(), row, f.name(), opts),
                );
            }
            Value::Object(obj)
        }
        DataType::Dictionary(_, value) => match cast(&arr.slice(row, 1), value) {
            Ok(decoded) => cell_json_with(decoded.as_ref(), 0, name, opts),
            Err(_) => Value::Null,
        },
        _ => match ArrayFormatter::try_new(arr, &FormatOptions::default()) {
            Ok(f) => Value::String(f.value(row).to_string()),
            Err(_) => Value::Null,
        },
    }
}

/// Text form of a JSON cell: strings raw, null empty, arrays joined.
pub fn value_text(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Array(items) => items.iter().map(value_text).collect::<Vec<_>>().join(", "),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_with_ellipsis() {
        let s = "x".repeat(100);
        let t = truncate(&s, 80);
        assert_eq!(t.chars().count(), 80);
        assert!(t.ends_with('…'));
        assert_eq!(truncate("short", 80), "short");
    }

    #[test]
    fn csv_escaping() {
        assert_eq!(csv_escape("a,b"), "\"a,b\"");
        assert_eq!(csv_escape("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(csv_escape("plain"), "plain");
    }

    #[test]
    fn csv_cells_that_a_spreadsheet_would_run_are_neutralised() {
        use datafusion::arrow::array::{Int64Array, StringArray};
        let batch = RecordBatch::try_from_iter([
            (
                "text",
                Arc::new(StringArray::from(vec![
                    "=HYPERLINK(\"http://evil\",\"x\")",
                    "+1+1",
                    "-2+3",
                    "@SUM(A1)",
                    "\tTAB",
                    "\rCR",
                    "plain, with comma",
                    "ok=fine",
                ])) as ArrayRef,
            ),
            (
                "n",
                Arc::new(Int64Array::from(vec![-5, 1, 2, 3, 4, 5, 6, 7])) as ArrayRef,
            ),
        ])
        .unwrap();
        let r = QueryResult::new(batch.schema(), vec![batch], ResultKind::Rows, Vec::new());
        let csv = r.render_csv();
        let lines: Vec<&str> = csv.split('\n').collect();
        assert_eq!(lines[0], "text,n");
        assert_eq!(
            lines[1],
            "\"'=HYPERLINK(\"\"http://evil\"\",\"\"x\"\")\",-5"
        );
        assert_eq!(lines[2], "'+1+1,1");
        assert_eq!(lines[3], "'-2+3,2");
        assert_eq!(lines[4], "'@SUM(A1),3");
        assert!(csv.contains("'\tTAB,4"), "{csv:?}");
        assert!(csv.contains("\"'\rCR\",5"), "{csv:?}");
        assert!(csv.contains("\"plain, with comma\",6"));
        // Only a leading character counts; numbers stay numbers (-5 above).
        assert!(csv.contains("ok=fine,7"));
        // The table and JSON renderings are not touched.
        assert!(
            r.to_json()[0]["text"]
                .as_str()
                .unwrap()
                .starts_with("=HYPERLINK")
        );
        assert!(r.render_table(None).contains("=HYPERLINK"));
    }

    #[test]
    fn f32_renders_short() {
        assert_eq!(f32_short(0.9), Value::from(0.9));
    }
}
