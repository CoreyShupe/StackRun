use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;
use crossterm::{
    cursor, execute, queue,
    style::{Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor},
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};

struct Term {
    name: String,
    cmd: String,
    parser: Arc<Mutex<vt100::Parser>>,
    writer: Box<dyn Write + Send>,
    master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
    exited: Arc<AtomicBool>,
}

fn spawn_term(name: String, cmd: String, rows: u16, cols: u16) -> Result<Term> {
    let pty_system = native_pty_system();
    let pair = pty_system.openpty(PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    })?;

    let mut cb = CommandBuilder::new("bash");
    cb.args(["-c", cmd.as_str()]);
    cb.env("TERM", "xterm-256color");
    if let Ok(cwd) = std::env::current_dir() {
        cb.cwd(cwd);
    }
    let child = pair.slave.spawn_command(cb)?;
    drop(pair.slave);

    let parser = Arc::new(Mutex::new(vt100::Parser::new(rows, cols, 0)));
    let reader = pair.master.try_clone_reader()?;
    let writer = pair.master.take_writer()?;

    let parser_clone = parser.clone();
    let exited = Arc::new(AtomicBool::new(false));
    let exited_clone = exited.clone();
    thread::spawn(move || {
        let mut reader = reader;
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut p = parser_clone.lock().unwrap();
                    p.process(&buf[..n]);
                }
            }
        }
        exited_clone.store(true, Ordering::SeqCst);
    });

    Ok(Term {
        name,
        cmd,
        parser,
        writer,
        master: pair.master,
        child,
        exited,
    })
}

fn restart_term(t: &mut Term, rows: u16, cols: u16) -> Result<()> {
    let _ = t.child.kill();
    let _ = t.child.wait();
    let fresh = spawn_term(t.name.clone(), t.cmd.clone(), rows, cols)?;
    t.parser = fresh.parser;
    t.writer = fresh.writer;
    t.master = fresh.master;
    t.child = fresh.child;
    t.exited = fresh.exited;
    Ok(())
}

fn vt_color(c: vt100::Color) -> Color {
    use vt100::Color as VC;
    match c {
        VC::Default => Color::Reset,
        VC::Idx(i) => Color::AnsiValue(i),
        VC::Rgb(r, g, b) => Color::Rgb { r, g, b },
    }
}

fn render_title(
    out: &mut impl Write,
    terms: &[Term],
    current: usize,
    cols: u16,
) -> Result<()> {
    queue!(
        out,
        cursor::MoveTo(0, 0),
        ResetColor,
        SetAttribute(Attribute::Reset),
        Clear(ClearType::CurrentLine),
    )?;

    let mut pieces: Vec<(bool, String)> = Vec::new();
    pieces.push((false, " StackRun ".to_string()));
    for (i, t) in terms.iter().enumerate() {
        let dead = t.exited.load(Ordering::SeqCst);
        let mark = if dead { " ✓" } else { "" };
        pieces.push((i == current, format!(" {}{} ", t.name, mark)));
    }
    pieces.push((false, "  Alt+Tab to cycle ".to_string()));

    let mut written = 0usize;
    let max = cols as usize;
    for (active, piece) in &pieces {
        if written >= max {
            break;
        }
        if *active {
            queue!(
                out,
                SetBackgroundColor(Color::White),
                SetForegroundColor(Color::Black),
                SetAttribute(Attribute::Bold)
            )?;
        } else {
            queue!(
                out,
                SetBackgroundColor(Color::DarkBlue),
                SetForegroundColor(Color::White),
                SetAttribute(Attribute::Reset)
            )?;
        }
        for ch in piece.chars() {
            if written >= max {
                break;
            }
            queue!(out, Print(ch))?;
            written += 1;
        }
    }
    queue!(
        out,
        SetBackgroundColor(Color::DarkBlue),
        SetForegroundColor(Color::White),
        SetAttribute(Attribute::Reset)
    )?;
    while written < max {
        queue!(out, Print(' '))?;
        written += 1;
    }
    queue!(out, ResetColor, SetAttribute(Attribute::Reset))?;
    Ok(())
}

fn render(
    out: &mut impl Write,
    terms: &[Term],
    current: usize,
    cols: u16,
) -> Result<()> {
    queue!(out, cursor::Hide)?;
    render_title(out, terms, current, cols)?;

    let term = &terms[current];
    let parser = term.parser.lock().unwrap();
    let screen = parser.screen();
    let (rows, scols) = screen.size();

    let mut last_fg = Color::Reset;
    let mut last_bg = Color::Reset;
    let mut bold = false;
    let mut italic = false;
    let mut underline = false;
    let mut inverse = false;
    queue!(out, ResetColor, SetAttribute(Attribute::Reset))?;

    for row in 0..rows {
        queue!(out, cursor::MoveTo(0, row + 1))?;
        let mut col: u16 = 0;
        while col < scols {
            let cell = screen.cell(row, col);
            match cell {
                Some(c) => {
                    let fg = vt_color(c.fgcolor());
                    let bg = vt_color(c.bgcolor());

                    if fg != last_fg {
                        queue!(out, SetForegroundColor(fg))?;
                        last_fg = fg;
                    }
                    if bg != last_bg {
                        queue!(out, SetBackgroundColor(bg))?;
                        last_bg = bg;
                    }
                    let cb = c.bold();
                    if cb != bold {
                        queue!(
                            out,
                            SetAttribute(if cb { Attribute::Bold } else { Attribute::NormalIntensity })
                        )?;
                        bold = cb;
                    }
                    let ci = c.italic();
                    if ci != italic {
                        queue!(
                            out,
                            SetAttribute(if ci { Attribute::Italic } else { Attribute::NoItalic })
                        )?;
                        italic = ci;
                    }
                    let cu = c.underline();
                    if cu != underline {
                        queue!(
                            out,
                            SetAttribute(if cu { Attribute::Underlined } else { Attribute::NoUnderline })
                        )?;
                        underline = cu;
                    }
                    let cinv = c.inverse();
                    if cinv != inverse {
                        queue!(
                            out,
                            SetAttribute(if cinv { Attribute::Reverse } else { Attribute::NoReverse })
                        )?;
                        inverse = cinv;
                    }

                    let contents = c.contents();
                    if c.is_wide() {
                        if contents.is_empty() {
                            queue!(out, Print("  "))?;
                        } else {
                            queue!(out, Print(contents))?;
                        }
                        col += 2;
                    } else {
                        if contents.is_empty() {
                            queue!(out, Print(' '))?;
                        } else {
                            queue!(out, Print(contents))?;
                        }
                        col += 1;
                    }
                }
                None => {
                    queue!(out, Print(' '))?;
                    col += 1;
                }
            }
        }
    }

    queue!(out, ResetColor, SetAttribute(Attribute::Reset))?;
    let (crow, ccol) = screen.cursor_position();
    queue!(out, cursor::MoveTo(ccol, crow + 1))?;
    if !screen.hide_cursor() {
        queue!(out, cursor::Show)?;
    }

    out.flush()?;
    Ok(())
}

fn summarize(cmd: &str) -> String {
    let trimmed = cmd.trim();
    let max = 32;
    let chars: Vec<char> = trimmed.chars().collect();
    if chars.len() <= max {
        trimmed.to_string()
    } else {
        let s: String = chars.iter().take(max - 1).collect();
        format!("{}…", s)
    }
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), LeaveAlternateScreen, cursor::Show);
        let _ = terminal::disable_raw_mode();
    }
}

fn main() -> Result<()> {
    let cmds: Vec<String> = std::env::args().skip(1).collect();
    if cmds.is_empty() {
        eprintln!("Usage: stackrun <cmd1> [cmd2] [cmd3] ...");
        eprintln!();
        eprintln!("Each argument is run as a separate `bash -c` command in its own PTY.");
        eprintln!("Alt+Tab cycles through them. All other input goes to the visible one.");
        std::process::exit(1);
    }

    let (cols0, rows0) = terminal::size().unwrap_or((80, 24));
    let cols = cols0.max(2);
    let sub_rows = rows0.saturating_sub(1).max(2);

    terminal::enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen, cursor::Hide)?;
    let _guard = TerminalGuard;

    let mut terms: Vec<Term> = Vec::new();
    for (i, c) in cmds.iter().enumerate() {
        let name = format!("[{}] {}", i + 1, summarize(c));
        match spawn_term(name, c.clone(), sub_rows, cols) {
            Ok(t) => terms.push(t),
            Err(e) => {
                drop(_guard);
                eprintln!("Failed to spawn command {:?}: {}", c, e);
                std::process::exit(1);
            }
        }
    }

    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    thread::spawn(move || {
        let tty = std::fs::OpenOptions::new().read(true).open("/dev/tty");
        if let Ok(mut tty) = tty {
            let mut buf = [0u8; 1024];
            loop {
                match tty.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        }
    });

    let mut stdout = io::stdout();
    let _ = run_loop(&mut stdout, &mut terms, rx, cols, sub_rows);

    for t in terms.iter_mut() {
        let _ = t.child.kill();
    }

    Ok(())
}

fn run_loop(
    stdout: &mut io::Stdout,
    terms: &mut Vec<Term>,
    rx: mpsc::Receiver<Vec<u8>>,
    mut cols: u16,
    mut sub_rows: u16,
) -> Result<()> {
    let mut current: usize = 0;
    let mut last_render = Instant::now() - Duration::from_secs(1);
    let render_interval = Duration::from_millis(33);
    let mut pending_esc_at: Option<Instant> = None;
    let mut force_redraw = true;

    loop {
        let (raw_cols, raw_rows) = terminal::size().unwrap_or((cols, sub_rows + 1));
        let new_cols = raw_cols.max(2);
        let new_sub_rows = raw_rows.saturating_sub(1).max(2);
        if new_cols != cols || new_sub_rows != sub_rows {
            cols = new_cols;
            sub_rows = new_sub_rows;
            for t in terms.iter() {
                let _ = t.master.resize(PtySize {
                    rows: sub_rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                });
                let mut p = t.parser.lock().unwrap();
                p.screen_mut().set_size(sub_rows, cols);
            }
            queue!(stdout, Clear(ClearType::All))?;
            force_redraw = true;
        }

        let mut had_input = false;
        while let Ok(bytes) = rx.try_recv() {
            had_input = true;
            let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
            let bs = &bytes[..];
            let mut i = 0;

            if pending_esc_at.is_some() {
                pending_esc_at = None;
                if !bs.is_empty() && bs[0] == b'\t' {
                    current = (current + 1) % terms.len();
                    force_redraw = true;
                    queue!(stdout, Clear(ClearType::All))?;
                    i = 1;
                } else {
                    out.push(0x1b);
                }
            }

            while i < bs.len() {
                if bs[i] == 0x1b {
                    if i + 1 < bs.len() {
                        if bs[i + 1] == b'\t' {
                            current = (current + 1) % terms.len();
                            force_redraw = true;
                            queue!(stdout, Clear(ClearType::All))?;
                            i += 2;
                        } else {
                            out.push(bs[i]);
                            i += 1;
                        }
                    } else {
                        pending_esc_at = Some(Instant::now());
                        i += 1;
                    }
                } else if bs[i] == 0x12 && terms[current].exited.load(Ordering::SeqCst) {
                    let _ = restart_term(&mut terms[current], sub_rows, cols);
                    out.clear();
                    force_redraw = true;
                    queue!(stdout, Clear(ClearType::All))?;
                    i += 1;
                } else {
                    out.push(bs[i]);
                    i += 1;
                }
            }

            if !out.is_empty() {
                let _ = terms[current].writer.write_all(&out);
                let _ = terms[current].writer.flush();
            }
        }

        if let Some(t) = pending_esc_at {
            if t.elapsed() > Duration::from_millis(80) {
                let _ = terms[current].writer.write_all(&[0x1b]);
                let _ = terms[current].writer.flush();
                pending_esc_at = None;
            }
        }

        if force_redraw || last_render.elapsed() >= render_interval {
            render(stdout, terms, current, cols)?;
            last_render = Instant::now();
            force_redraw = false;
        }

        if terms.iter().all(|t| t.exited.load(Ordering::SeqCst)) {
            render(stdout, terms, current, cols)?;
            thread::sleep(Duration::from_millis(150));
            break;
        }

        if !had_input {
            thread::sleep(Duration::from_millis(15));
        }
    }
    Ok(())
}
