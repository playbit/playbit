use super::*;
use crate::{TestAppContext, WindowTextSystem, font, hsla};

fn decorated_run(index: usize, len: usize) -> TextRun {
    let color = hsla(index as f32 / 64., 1., 0.5, 1.);
    TextRun {
        len,
        letter_spacing: px(0.),
        font: font("Helvetica"),
        color,
        background_color: Some(color),
        underline: Some(UnderlineStyle {
            thickness: px(1.),
            color: Some(color),
            wavy: index % 2 == 0,
        }),
        strikethrough: Some(StrikethroughStyle { thickness: px(2.), color: Some(color) }),
    }
}

fn assert_decoration(actual: &DecorationRun, expected: &TextRun) {
    assert_eq!(actual.len, 2, "each retained Unicode character is two UTF-8 bytes");
    assert_eq!(actual.color, expected.color);
    assert_eq!(actual.background_color, expected.background_color);
    assert_eq!(actual.underline, expected.underline);
    assert_eq!(actual.strikethrough, expected.strikethrough);
}

#[crate::test]
fn playbit_compact_decoration_storage_preserves_inline_and_spilled_lines(cx: &mut TestAppContext) {
    // Exercise actual shaping across the inline limit and former inline size.
    cx.update(|cx| {
        let system = WindowTextSystem::new(cx.text_system().clone());
        for count in [1, 2, 3, 64] {
            let text: SharedString = "é".repeat(count).into();
            let runs: Vec<_> = (0..count).map(|index| decorated_run(index, 2)).collect();
            let line = system.shape_line(text.clone(), px(16.), &runs, None);
            assert_eq!(line.text, text);
            assert_eq!(line.decoration_runs.len(), count);
            assert_eq!(line.decoration_runs.spilled(), count > 2);
            for (actual, expected) in line.decoration_runs.iter().zip(&runs) {
                assert_decoration(actual, expected);
            }
            let cloned = line.clone();
            drop(line);
            for (actual, expected) in cloned.decoration_runs.iter().zip(&runs) {
                assert_decoration(actual, expected);
            }
        }
        assert!(std::mem::size_of::<ShapedLine>() <= 512, "ordinary labels retain compact state");
        assert!(std::mem::size_of::<WrappedLine>() <= 512, "wrapped labels retain compact state");
    });
}

#[crate::test]
fn playbit_compact_decoration_storage_preserves_rich_multiline_text(cx: &mut TestAppContext) {
    // A style run crosses a newline; both resulting lines spill and keep every style.
    cx.update(|cx| {
        let system = WindowTextSystem::new(cx.text_system().clone());
        let text: SharedString = format!("{}\n{}", "é".repeat(32), "é".repeat(32)).into();
        let runs: Vec<_> =
            (0..64).map(|index| decorated_run(index, if index == 31 { 3 } else { 2 })).collect();
        let lines = system.shape_text(text, px(16.), &runs, Some(px(4096.)), None).unwrap();
        assert_eq!(lines.len(), 2);
        for (index, line) in lines.iter().enumerate() {
            assert_eq!(line.text.as_ref(), "é".repeat(32));
            assert_eq!(line.decoration_runs.len(), 32);
            assert!(line.decoration_runs.spilled());
            for (actual, expected) in line.decoration_runs.iter().zip(&runs[index * 32..]) {
                assert_decoration(actual, expected);
            }
        }
    });
}
