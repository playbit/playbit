//! Resource owners publish changed numeric observations as ordinary events.
#[derive(Default)]
pub struct Gauge {
    name: Option<&'static str>,
    value: u64,
}
impl Gauge {
    pub fn new(name: &'static str, initial: u64) -> Self {
        crate::mark_value(name, initial);
        Self { name: Some(name), value: initial }
    }
    pub fn set(&mut self, value: u64) {
        if self.value != value {
            self.value = value;
            if let Some(name) = self.name {
                crate::mark_value(name, value);
            }
        }
    }
}
