use polars::prelude::*;

#[derive(Debug, Clone, PartialEq)]
pub enum FieldType {
    Time,
    Float64,
    String,
}

#[derive(Debug, Clone)]
pub struct Field {
    pub name: String,
    pub ty: FieldType,
    pub series: Series,
}

impl Field {
    pub fn new(name: &str, ty: FieldType, values: Vec<f64>) -> Self {
        let series = Series::new(name.into(), values);
        Self {
            name: name.to_string(),
            ty,
            series,
        }
    }

    /// A `String`-typed field backed by a Polars string series (log lines, text
    /// columns). `Field::new` only builds numeric (`f64`) series.
    pub fn new_str(name: &str, values: Vec<String>) -> Self {
        let series = Series::new(name.into(), values);
        Self {
            name: name.to_string(),
            ty: FieldType::String,
            series,
        }
    }

    /// The field's numeric values as `f64`, skipping nulls. Empty when the
    /// backing series isn't `f64` (Time/Float64 fields are both f64-backed).
    pub fn floats(&self) -> Vec<f64> {
        self.series
            .f64()
            .map(|ca| ca.into_iter().flatten().collect())
            .unwrap_or_default()
    }

    /// The field's values as owned `String`s (nulls → empty). Empty when the
    /// backing series isn't a string series.
    pub fn strings(&self) -> Vec<String> {
        self.series
            .str()
            .map(|ca| {
                ca.into_iter()
                    .map(|o| o.unwrap_or("").to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Number of values in the series.
    pub fn len(&self) -> usize {
        self.series.len()
    }

    pub fn is_empty(&self) -> bool {
        self.series.is_empty()
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
}
