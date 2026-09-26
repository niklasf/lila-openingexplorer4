use parquet::data_type::DataType;

pub struct OptionalColumn<T: DataType> {
    values: Vec<T::T>,
    def_levels: Vec<i16>,
}

impl<T: DataType> Default for OptionalColumn<T> {
    fn default() -> Self {
        Self {
            values: Vec::new(),
            def_levels: Vec::new(),
        }
    }
}

impl<T: DataType> OptionalColumn<T> {
    pub fn values(&self) -> &[T::T] {
        &self.values
    }

    pub fn def_levels(&self) -> &[i16] {
        &self.def_levels
    }

    pub fn push(&mut self, value: Option<T::T>) {
        match value {
            Some(value) => {
                self.values.push(value);
                self.def_levels.push(1);
            }
            None => self.def_levels.push(0),
        }
    }

    pub fn clear(&mut self) {
        self.values.clear();
        self.def_levels.clear();
    }
}
