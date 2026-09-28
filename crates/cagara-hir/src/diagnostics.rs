/// Diagnostic messages accumulated during lowering and type checking
#[salsa::accumulator]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub message: String,
    pub span: Option<(usize, usize)>,
}

impl Diagnostic {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            span: None,
        }
    }

    pub fn with_span(mut self, start: usize, end: usize) -> Self {
        self.span = Some((start, end));
        self
    }
}
