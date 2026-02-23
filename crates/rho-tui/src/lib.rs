mod autocomplete;
pub mod client;
mod editor;
mod keys;
mod overlay;
mod select_list;
mod settings_list;
mod stdin_buffer;
mod terminal;
mod theme;
mod widgets;

pub use client::{TuiClient, TuiClientError};
