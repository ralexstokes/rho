use std::io::{Stdout, Write, stdout};

use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

pub struct ProcessTerminal {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    kitty_protocol_active: bool,
}

impl ProcessTerminal {
    pub fn start() -> std::io::Result<Self> {
        enable_raw_mode()?;

        let mut stdout = stdout();
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
        stdout.write_all(b"\x1b[?2004h")?;
        stdout.write_all(b"\x1b[?u")?;
        stdout.flush()?;

        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend)?;
        Ok(Self {
            terminal,
            kitty_protocol_active: false,
        })
    }

    pub fn terminal_mut(&mut self) -> &mut Terminal<CrosstermBackend<Stdout>> {
        &mut self.terminal
    }

    pub fn clear(&mut self) -> std::io::Result<()> {
        self.terminal.clear()
    }

    pub fn width(&mut self) -> std::io::Result<u16> {
        Ok(self.terminal.size()?.width)
    }

    pub fn begin_synchronized_output(&mut self) -> std::io::Result<()> {
        self.terminal.backend_mut().write_all(b"\x1b[?2026h")?;
        self.terminal.backend_mut().flush()
    }

    pub fn end_synchronized_output(&mut self) -> std::io::Result<()> {
        self.terminal.backend_mut().write_all(b"\x1b[?2026l")?;
        self.terminal.backend_mut().flush()
    }
}

impl Drop for ProcessTerminal {
    fn drop(&mut self) {
        if self.kitty_protocol_active {
            let _ = self.terminal.backend_mut().write_all(b"\x1b[<u");
        }
        let _ = self.terminal.backend_mut().write_all(b"\x1b[?2004l");
        let _ = self.terminal.backend_mut().flush();
        let _ = disable_raw_mode();
        let _ = execute!(
            self.terminal.backend_mut(),
            LeaveAlternateScreen,
            DisableMouseCapture
        );
        let _ = self.terminal.show_cursor();
    }
}
