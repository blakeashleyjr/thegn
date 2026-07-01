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
