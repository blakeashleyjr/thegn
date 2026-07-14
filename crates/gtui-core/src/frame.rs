//! A minimal columnar data model for the gtui query/render pipeline.
//!
//! Each [`Field`] is a named column backed by either `f64` or `String` values.
//! This is deliberately a thin hand-rolled type rather than a dataframe library:
//! the renderers only ever need "the numbers" ([`Field::floats`]), "the strings"
//! ([`Field::strings`]), a length, and a per-cell display string
//! ([`Field::cell`]) — so pulling in polars (and arrow) for a `Series` wrapper
//! was pure build-time cost.

#[derive(Debug, Clone, PartialEq)]
pub enum FieldType {
    Time,
    Float64,
    String,
}

/// A column's backing values: numeric (`f64`) or text.
#[derive(Debug, Clone)]
enum Column {
    F64(Vec<f64>),
    Str(Vec<String>),
}

impl Column {
    fn len(&self) -> usize {
        match self {
            Column::F64(v) => v.len(),
            Column::Str(v) => v.len(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Field {
    pub name: String,
    pub ty: FieldType,
    data: Column,
}

impl Field {
    /// A numeric (`f64`) field. Both `Time` and `Float64` columns are f64-backed.
    pub fn new(name: &str, ty: FieldType, values: Vec<f64>) -> Self {
        Self {
            name: name.to_string(),
            ty,
            data: Column::F64(values),
        }
    }

    /// A `String`-typed field (log lines, text columns). `Field::new` only builds
    /// numeric (`f64`) columns.
    pub fn new_str(name: &str, values: Vec<String>) -> Self {
        Self {
            name: name.to_string(),
            ty: FieldType::String,
            data: Column::Str(values),
        }
    }

    /// The field's numeric values as `f64`. Empty when the column is text-backed
    /// (`Time`/`Float64` fields are both f64-backed).
    pub fn floats(&self) -> Vec<f64> {
        match &self.data {
            Column::F64(v) => v.clone(),
            Column::Str(_) => Vec::new(),
        }
    }

    /// The field's values as owned `String`s. Empty when the column is numeric.
    pub fn strings(&self) -> Vec<String> {
        match &self.data {
            Column::Str(v) => v.clone(),
            Column::F64(_) => Vec::new(),
        }
    }

    /// The value at row `i` formatted for display (empty string when out of
    /// range). Used for naive table-cell rendering.
    pub fn cell(&self, i: usize) -> String {
        match &self.data {
            Column::F64(v) => v.get(i).map(|x| x.to_string()).unwrap_or_default(),
            Column::Str(v) => v.get(i).cloned().unwrap_or_default(),
        }
    }

    /// Number of values in the column.
    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.len() == 0
    }
}

#[derive(Debug, Clone)]
pub struct Frame {
    pub fields: Vec<Field>,
}

impl Frame {
    pub fn new(fields: Vec<Field>) -> Self {
        Self { fields }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_frame_creation() {
        let field = Field::new("value", FieldType::Float64, vec![1.0, 2.0, 3.0]);
        let frame = Frame::new(vec![field]);
        assert_eq!(frame.fields.len(), 1);
        assert_eq!(frame.fields[0].name, "value");
    }

    #[test]
    fn numeric_and_string_columns() {
        let nums = Field::new("v", FieldType::Float64, vec![1.0, 2.5]);
        assert_eq!(nums.floats(), vec![1.0, 2.5]);
        assert!(nums.strings().is_empty());
        assert_eq!(nums.len(), 2);
        assert!(!nums.is_empty());
        assert_eq!(nums.cell(1), "2.5");
        assert_eq!(nums.cell(9), "");

        let txt = Field::new_str("line", vec!["a".into(), "b".into()]);
        assert_eq!(txt.strings(), vec!["a".to_string(), "b".to_string()]);
        assert!(txt.floats().is_empty());
        assert_eq!(txt.cell(0), "a");
    }
}
