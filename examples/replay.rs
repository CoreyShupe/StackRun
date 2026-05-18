use std::io::Read;
fn main() {
    let mut args = std::env::args().skip(1);
    let cols: u16 = args.next().expect("cols").parse().unwrap();
    let rows: u16 = args.next().expect("rows").parse().unwrap();
    let path = args.next().expect("path");
    let mut data = Vec::new();
    std::fs::File::open(&path).unwrap().read_to_end(&mut data).unwrap();
    let mut p = vt100::Parser::new(rows, cols, 0);
    p.process(&data);
    let s = p.screen();
    let (r, c) = s.size();
    for row in 0..r {
        let mut line = String::new();
        let mut col = 0u16;
        while col < c {
            if let Some(cell) = s.cell(row, col) {
                let cs = cell.contents();
                if cs.is_empty() {
                    line.push(' ');
                } else {
                    line.push_str(&cs);
                }
                col += if cell.is_wide() { 2 } else { 1 };
            } else {
                line.push(' ');
                col += 1;
            }
        }
        println!("{}", line.trim_end());
    }
    let (cr, cc) = s.cursor_position();
    eprintln!("cursor at row={} col={}", cr, cc);
}
