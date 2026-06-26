use ratatui::text::Span;
fn main() {
    let s = Span::raw("test\r");
    println!("{:?}", s.content);
}
