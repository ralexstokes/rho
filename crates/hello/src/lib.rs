//! Placeholder crate so the template workspace builds and the CI checks have
//! something to run against. Replace with real crates.

/// Returns a greeting for `name`.
///
/// ```
/// assert_eq!(hello::greet("world"), "hello, world");
/// ```
pub fn greet(name: &str) -> String {
    format!("hello, {name}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn greets_by_name() {
        assert_eq!(greet("rho"), "hello, rho");
    }
}
