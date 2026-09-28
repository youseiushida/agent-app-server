//! Terminal QR codes (two rows per text line with half blocks).

use qrcode::QrCode;

/// Renders `data` as a QR code with a quiet zone, dark modules as `█`.
pub fn render(data: &str) -> anyhow::Result<String> {
    let code = QrCode::new(data.as_bytes())?;
    let width = code.width();
    let dark = |x: isize, y: isize| -> bool {
        if x < 0 || y < 0 || x >= width as isize || y >= width as isize {
            return false;
        }
        code[(x as usize, y as usize)] == qrcode::Color::Dark
    };
    let quiet = 2isize;
    let mut out = String::new();
    let mut y = -quiet;
    while y < width as isize + quiet {
        for x in -quiet..width as isize + quiet {
            out.push(match (dark(x, y), dark(x, y + 1)) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        out.push('\n');
        y += 2;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn renders_square_output() {
        let s = super::render("aas://pair?u=ws%3A%2F%2Fx&c=ABCD-EFGH&n=pc").unwrap();
        let lines: Vec<&str> = s.lines().collect();
        assert!(lines.len() > 10);
        let w = lines[0].chars().count();
        assert!(lines.iter().all(|l| l.chars().count() == w));
    }
}
