pub mod claude_code_panel;

pub use claude_code_panel::*;

use gpui::App;

pub fn init(cx: &mut App) {
    claude_code_panel::init(cx);
}
