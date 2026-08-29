//! Query results and their renderings (JSON, table, CSV).

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
}

impl QueryResult {
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
        }
    }

    /// An empty result carrying the schema the rows would have had.
    pub fn empty(schema: SchemaRef, note: impl Into<String>) -> Self {
        Self {
            schema,
            batches: Vec::new(),
            kind: ResultKind::Empty,
            notes: vec![note.into()],
        }
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
        let names = self.column_names();
        let mut rows = Vec::with_capacity(self.row_count());
        for batch in &self.batches {
            let cols = decoded_columns(batch);
            for row in 0..batch.num_rows() {
                let mut obj = Map::with_capacity(names.len());
                for (i, name) in names.iter().enumerate() {
                    obj.insert(name.clone(), cell_json(cols[i].as_ref(), row, name));
                }
                rows.push(Value::Object(obj));
            }
        }
        Value::Array(rows)
    }

    /// Every row as text cells (same conversions as [`Self::to_json`], with
    /// nulls as empty strings and lists joined by `, `).
    pub fn cells(&self) -> Vec<Vec<String>> {
        let names = self.column_names();
        let mut rows = Vec::with_capacity(self.row_count());
        for batch in &self.batches {
            let cols = decoded_columns(batch);
            for row in 0..batch.num_rows() {
                rows.push(
                    names
                        .iter()
                        .enumerate()
                        .map(|(i, name)| value_text(&cell_json(cols[i].as_ref(), row, name)))
                        .collect(),
                );
            }
        }
        rows
    }

    /// A comfy-table rendering with a `(n rows)` footer; long cells are
    /// truncated at 80 characters. `max_width` enables dynamic column
    /// wrapping to that many characters. `notes` are not included: callers
    /// print them after the table.
    pub fn render_table(&self, max_width: Option<usize>) -> String {
        let mut table = Table::new();
        table.load_preset(presets::UTF8_FULL_CONDENSED);
        // Wrap to `max_width` only while every column keeps a readable
        // minimum; otherwise let the table run wide rather than squeezing
        // twenty columns into four characters each.
        let columns = self.schema.fields().len().max(1);
        if let Some(w) = max_width
            && w / columns >= MIN_COLUMN_WIDTH
        {
            table.set_content_arrangement(ContentArrangement::Dynamic);
            table.set_width(w.clamp(20, usize::from(u16::MAX)) as u16);
        }
        table.set_header(self.column_names());
        for row in self.cells() {
            table.add_row(row.iter().map(|c| truncate(c, CELL_LIMIT)));
        }
        let n = self.row_count();
        let mut out = String::new();
        if !self.schema.fields().is_empty() {
            out.push_str(&table.to_string());
            out.push('\n');
        }
        out.push_str(&format!("({n} row{})", if n == 1 { "" } else { "s" }));
        out
    }

    /// RFC 4180-style CSV with a header row.
    pub fn render_csv(&self) -> String {
        let mut out = String::new();
        out.push_str(
            &self
                .column_names()
                .iter()
                .map(|c| csv_escape(c))
                .collect::<Vec<_>>()
                .join(","),
        );
        out.push('\n');
        for row in self.cells() {
            out.push_str(
                &row.iter()
                    .map(|c| csv_escape(c))
                    .collect::<Vec<_>>()
                    .join(","),
            );
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
pub fn cell_json(arr: &dyn Array, row: usize, name: &str) -> Value {
    if row >= arr.len() || arr.is_null(row) {
        return Value::Null;
    }
    match arr.data_type() {
        DataType::Null => Value::Null,
        DataType::Utf8 => Value::String(arr.as_string::<i32>().value(row).to_string()),
        DataType::LargeUtf8 => Value::String(arr.as_string::<i64>().value(row).to_string()),
        DataType::Utf8View => Value::String(arr.as_string_view().value(row).to_string()),
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
                    .map(|i| cell_json(inner.as_ref(), i, name))
                    .collect(),
            )
        }
        DataType::LargeList(_) => {
            let inner = arr.as_list::<i64>().value(row);
            Value::Array(
                (0..inner.len())
                    .map(|i| cell_json(inner.as_ref(), i, name))
                    .collect(),
            )
        }
        DataType::Struct(fields) => {
            let s = arr.as_struct();
            let mut obj = Map::new();
            for (i, f) in fields.iter().enumerate() {
                obj.insert(
                    f.name().clone(),
                    cell_json(s.column(i).as_ref(), row, f.name()),
                );
            }
            Value::Object(obj)
        }
        DataType::Dictionary(_, value) => match cast(&arr.slice(row, 1), value) {
            Ok(decoded) => cell_json(decoded.as_ref(), 0, name),
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
    fn f32_renders_short() {
        assert_eq!(f32_short(0.9), Value::from(0.9));
    }
}
