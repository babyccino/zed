use std::ops::Range;

use crate::{DisplayPoint, DisplayRow, Editor, EditorSnapshot, EditorStyle};
use gpui::{AnyElement, HighlightStyle, IntoElement, StyledText, ViewContext};

#[derive(Debug, Clone, Default)]
pub struct Overlay {
    pub text: String,
    pub highlights: Vec<(Range<usize>, HighlightStyle)>,
    pub point: DisplayPoint,
    pub offset: f32,
}

impl Overlay {
    pub fn render(
        &self,
        style: &EditorStyle,
        visible_display_row_range: Range<DisplayRow>,
    ) -> Option<(DisplayPoint, f32, AnyElement)> {
        if !visible_display_row_range.contains(&self.point.row()) {
            return None;
        }
        let iter = self.highlights.iter().cloned();

        let el = StyledText::new(self.text.clone())
            .with_highlights(&style.text, iter)
            .into_any_element();
        Some((self.point, self.offset, el))
    }
}