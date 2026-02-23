const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub fn loader_frame(tick: u64) -> &'static str {
    let index = (tick as usize) % SPINNER_FRAMES.len();
    SPINNER_FRAMES[index]
}
