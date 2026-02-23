#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TuiClient {
    url: String,
}

impl TuiClient {
    pub fn new(url: impl Into<String>) -> Self {
        Self { url: url.into() }
    }

    pub fn url(&self) -> &str {
        &self.url
    }
}
