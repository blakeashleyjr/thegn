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

    /// The field's numeric values, borrowed. Empty when the column is text-backed
    /// (`Time`/`Float64` fields are both f64-backed). Never copies.
    pub fn floats(&self) -> &[f64] {
        match &self.data {
            Column::F64(v) => v,
            Column::Str(_) => &[],
        }
    }

    /// The field's values, borrowed. Empty when the column is numeric.
    pub fn strings(&self) -> &[String] {
        match &self.data {
            Column::Str(v) => v,
            Column::F64(_) => &[],
        }
    }

    /// The value at row `i` formatted for display (empty string when out of
    /// range).
    pub fn cell(&self, i: usize) -> String {
        self.cell_str(i).into_owned()
    }

    /// Like [`Field::cell`] but borrows text cells instead of cloning them;
    /// only numeric cells allocate (they must be formatted).
    pub fn cell_str(&self, i: usize) -> std::borrow::Cow<'_, str> {
        use std::borrow::Cow;
        match &self.data {
            Column::F64(v) => v
                .get(i)
                .map(|x| Cow::Owned(x.to_string()))
                .unwrap_or(Cow::Borrowed("")),
            Column::Str(v) => v.get(i).map_or(Cow::Borrowed(""), |s| Cow::Borrowed(s)),
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
        assert_eq!(nums.floats(), &[1.0, 2.5][..]);
        assert!(nums.strings().is_empty());
        assert_eq!(nums.len(), 2);
        assert!(!nums.is_empty());
        assert_eq!(nums.cell(1), "2.5");
        assert_eq!(nums.cell(9), "");

        let txt = Field::new_str("line", vec!["a".into(), "b".into()]);
        assert_eq!(txt.strings(), &["a".to_string(), "b".to_string()][..]);
        assert!(matches!(txt.cell_str(0), std::borrow::Cow::Borrowed("a")));
        assert_eq!(txt.cell_str(7), "");
        assert!(txt.floats().is_empty());
        assert_eq!(txt.cell(0), "a");
    }
}
