//! Deterministic session state, reducers, context assembly, and commands.
//!
//! This crate is the pure decision-making core. Hosts execute the effects and
//! actions it emits and feed their outcomes back into the state machine.

#[cfg(test)]
mod tests {
    #[test]
    fn core_crate_links_without_a_shell() {}
}
