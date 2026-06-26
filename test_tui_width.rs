use unicode_width::UnicodeWidthStr;
use unicode_segmentation::UnicodeSegmentation;

fn grapheme_width(grapheme: &str) -> u8 {
    grapheme.width() as u8
}

fn str_len(s: &str) -> usize {
    s.graphemes(true).map(|g| grapheme_width(g) as usize).sum()
}

fn main() {
    let s = "🇺🇸";
    println!("UnicodeWidthStr: {}", s.width());
    println!("str_len: {}", str_len(s));
}
