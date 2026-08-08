// temporary scratch - verify fix: box width must keep content >= advance + 10
fn main() {
    use screenshot_rs::overlay::commands::{measure_line_advance_px, measure_text_px};
    use screenshot_rs::overlay::drawing::FontWeight;
    for s in ["你", "你好", "你好啊", "你好啊你", "你好啊你好", "你好啊你好啊", "你好啊你好啊你好"] {
        let (tw, _th, _, _) = measure_text_px(s, 24.0, None, FontWeight::Normal);
        let adv = measure_line_advance_px(s, 24.0, FontWeight::Normal);
        // 旧公式（原 bug 代码）
        let old_w = (tw / 1.0 + 16.0).max(100.0);
        let old_content = old_w - 18.0;
        let old_scroll = if old_content - 10.0 < adv { old_content - 10.0 - adv } else { 0.0 };
        // 新公式
        let new_w = (adv / 1.0 + 18.0 + 10.0).max(100.0);
        let new_content = new_w - 18.0;
        let new_scroll = if new_content - 10.0 < adv { new_content - 10.0 - adv } else { 0.0 };
        println!("{:?}: tw={:.1} adv={:.1} | old_w={:.1} content={:.1} scroll={:+.1}px | new_w={:.1} content={:.1} scroll={:+.1}px",
            s, tw, adv, old_w, old_content, old_scroll, new_w, new_content, new_scroll);
    }
    for s in ["a", "ab", "abc", "abcd", "abcde", "abcdef", "abcdefg"] {
        let (tw, _, _, _) = measure_text_px(s, 24.0, None, FontWeight::Normal);
        let adv = measure_line_advance_px(s, 24.0, FontWeight::Normal);
        let old_w = (tw / 1.0 + 16.0).max(100.0);
        let old_content = old_w - 18.0;
        let old_scroll = if old_content - 10.0 < adv { old_content - 10.0 - adv } else { 0.0 };
        let new_w = (adv / 1.0 + 18.0 + 10.0).max(100.0);
        let new_content = new_w - 18.0;
        let new_scroll = if new_content - 10.0 < adv { new_content - 10.0 - adv } else { 0.0 };
        println!("ASCII {:?}: tw={:.1} adv={:.1} | old_w={:.1} scroll={:+.1}px | new_w={:.1} scroll={:+.1}px",
            s, tw, adv, old_w, old_scroll, new_w, new_scroll);
    }
}
