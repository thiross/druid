// Copyright 2019 the Druid Authors
// SPDX-License-Identifier: Apache-2.0

//! Traits for text editing and a basic String implementation.

use std::borrow::Cow;
use std::ops::{Deref, Range};
use std::sync::Arc;

use druid_shell::text::Selection;
use unicode_segmentation::{GraphemeCursor, UnicodeSegmentation};

use crate::Data;

/// An opaque, document-local identifier for a composite text range.
///
/// Druid does not attach application semantics to the key. Text documents use
/// it to correlate a displayed composite range with their own data.
#[derive(Clone, Copy, Debug, Data, PartialEq, Eq, Hash)]
pub struct CompositeKey(u64);

impl CompositeKey {
    /// Create a new key.
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// Return the underlying value.
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A contiguous display range that behaves as a richer editing unit.
///
/// Druid does not interpret the range as an embedded widget, document node, or
/// application object. The [`EditableText`] implementation decides how carets,
/// selections, edits, and clipboard serialization interact with the range.
#[derive(Clone, Debug, Data, PartialEq, Eq)]
pub struct CompositeTextRange {
    /// A document-local key used to correlate this range with application data.
    pub key: CompositeKey,
    /// The range in the contiguous display text, in UTF-8 bytes.
    pub range: Range<usize>,
}

impl CompositeTextRange {
    /// Create a composite text range.
    pub const fn new(key: CompositeKey, range: Range<usize>) -> Self {
        Self { key, range }
    }

    /// Return `true` if `offset` is inside the displayed range.
    pub fn contains(&self, offset: usize) -> bool {
        self.range.start <= offset && offset < self.range.end
    }

    /// Return `true` if `offset` is strictly inside the displayed range.
    pub fn strictly_contains(&self, offset: usize) -> bool {
        self.range.start < offset && offset < self.range.end
    }
}

/// The direction in which a text position is being normalized.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextBoundary {
    /// Normalize to the upstream, lower-offset boundary.
    Upstream,
    /// Normalize to the downstream, higher-offset boundary.
    Downstream,
}

/// An invalid composite range reported by an [`EditableText`] document.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompositeTextRangeError {
    /// The range is empty.
    EmptyRange,
    /// The range starts before the previous composite or overlaps it.
    UnsortedOrOverlapping,
    /// The range extends past the document or is not on a character boundary.
    InvalidBoundary,
}

/// An EditableText trait.
pub trait EditableText: Sized {
    // TODO: would be nice to have something like
    // type Cursor: EditableTextCursor<Self>;

    /// Create a cursor with a reference to the text and a offset position.
    ///
    /// Returns None if the position isn't a codepoint boundary.
    fn cursor(&self, position: usize) -> Option<StringCursor<'_>>;

    /// Replace range with new text.
    /// Can panic if supplied an invalid range.
    // TODO: make this generic over Self
    fn edit(&mut self, range: Range<usize>, new: impl Into<String>);

    /// Get slice of text at range.
    fn slice(&self, range: Range<usize>) -> Option<Cow<'_, str>>;

    /// Get length of text (in bytes).
    fn len(&self) -> usize;

    /// Get the previous word offset from the given offset, if it exists.
    fn prev_word_offset(&self, offset: usize) -> Option<usize>;

    /// Get the next word offset from the given offset, if it exists.
    fn next_word_offset(&self, offset: usize) -> Option<usize>;

    /// Get the next grapheme offset from the given offset, if it exists.
    fn prev_grapheme_offset(&self, offset: usize) -> Option<usize>;

    /// Get the next grapheme offset from the given offset, if it exists.
    fn next_grapheme_offset(&self, offset: usize) -> Option<usize>;

    /// Get the previous codepoint offset from the given offset, if it exists.
    fn prev_codepoint_offset(&self, offset: usize) -> Option<usize>;

    /// Get the next codepoint offset from the given offset, if it exists.
    fn next_codepoint_offset(&self, offset: usize) -> Option<usize>;

    /// Get the preceding line break offset from the given offset
    fn preceding_line_break(&self, offset: usize) -> usize;

    /// Get the next line break offset from the given offset
    fn next_line_break(&self, offset: usize) -> usize;

    /// Returns `true` if this text has 0 length.
    fn is_empty(&self) -> bool;

    /// Construct an instance of this type from a `&str`.
    fn from_str(s: &str) -> Self;

    /// Return the composite text ranges in display order.
    ///
    /// Ranges must be non-overlapping, nonempty, and refer to UTF-8 ranges in
    /// the text returned by `TextStorage::as_str`.
    fn composite_ranges(&self) -> &[CompositeTextRange] {
        &[]
    }

    /// Return the composite containing `offset`, if any.
    fn composite_at(&self, offset: usize) -> Option<CompositeTextRange> {
        self.composite_ranges()
            .iter()
            .find(|composite| composite.contains(offset))
            .cloned()
    }

    /// Validate composite ordering, non-overlap, and UTF-8 boundaries.
    fn validate_composite_ranges(&self) -> Result<(), CompositeTextRangeError> {
        let mut previous_end = 0;
        for composite in self.composite_ranges() {
            if composite.range.is_empty() {
                return Err(CompositeTextRangeError::EmptyRange);
            }
            if composite.range.start < previous_end {
                return Err(CompositeTextRangeError::UnsortedOrOverlapping);
            }
            if composite.range.end > self.len()
                || self.cursor(composite.range.start).is_none()
                || self.cursor(composite.range.end).is_none()
            {
                return Err(CompositeTextRangeError::InvalidBoundary);
            }
            previous_end = composite.range.end;
        }
        Ok(())
    }

    /// Normalize a caret offset that falls inside a composite.
    ///
    /// The default implementation moves an internal offset to a composite
    /// boundary according to `affinity`. Documents that permit editing inside
    /// composites can override a less restrictive policy.
    fn normalize_caret_offset(&self, offset: usize, affinity: TextBoundary) -> usize {
        let Some(composite) = self
            .composite_ranges()
            .iter()
            .find(|composite| composite.strictly_contains(offset))
        else {
            return offset;
        };

        match affinity {
            TextBoundary::Upstream => composite.range.start,
            TextBoundary::Downstream => composite.range.end,
        }
    }

    /// Normalize selection endpoints so they do not split a composite.
    ///
    /// The default implementation selects a composite whole when a caret is
    /// placed inside it and expands partially intersecting selection endpoints.
    fn normalize_selection(&self, selection: Selection) -> Selection {
        let start = selection.min();
        let end = selection.max();

        if start == end {
            if let Some(composite) = self
                .composite_ranges()
                .iter()
                .find(|composite| composite.strictly_contains(start))
            {
                return Selection::new(composite.range.start, composite.range.end)
                    .with_h_pos(selection.h_pos);
            }
            return selection;
        }

        let (normalized_start, normalized_end) = self.normalize_range_boundaries(start..end);
        let anchor = if selection.anchor <= selection.active {
            normalized_start
        } else {
            normalized_end
        };
        let active = if selection.anchor <= selection.active {
            normalized_end
        } else {
            normalized_start
        };

        Selection::new(anchor, active).with_h_pos(selection.h_pos)
    }

    /// Expand an edit range so it never partially replaces a composite.
    fn normalize_edit_range(&self, range: Range<usize>) -> Range<usize> {
        let (start, end) = self.normalize_range_boundaries(range);
        start..end
    }

    /// Return the application-neutral clipboard text for `range`.
    ///
    /// The default implementation replaces each selected composite with the
    /// text returned by [`EditableText::composite_text`].
    fn clipboard_text(&self, range: Range<usize>) -> Cow<'_, str> {
        let Some(text) = self.slice(range.clone()) else {
            return Cow::Borrowed("");
        };

        let selected: Vec<_> = self
            .composite_ranges()
            .iter()
            .filter(|composite| {
                (composite.range.start < range.end && range.start < composite.range.end)
                    || (range.is_empty() && composite.contains(range.start))
            })
            .collect();

        if selected.is_empty() {
            return text;
        }

        let mut output = String::new();
        let mut cursor = range.start;
        for composite in selected {
            if cursor < composite.range.start {
                output.push_str(
                    self.slice(cursor..composite.range.start)
                        .unwrap_or_default()
                        .as_ref(),
                );
            }
            output.push_str(&self.composite_text(composite));
            cursor = composite.range.end;
        }
        if cursor < range.end {
            output.push_str(self.slice(cursor..range.end).unwrap_or_default().as_ref());
        }
        Cow::Owned(output)
    }

    /// Return the plain-text serialization of a composite.
    ///
    /// The default uses the displayed text. Application documents should
    /// override this method when display and clipboard forms differ.
    fn composite_text(&self, composite: &CompositeTextRange) -> Cow<'_, str> {
        self.slice(composite.range.clone()).unwrap_or_default()
    }

    /// Normalize the boundaries of `range` without changing zero-length ranges
    /// that do not intersect a composite.
    #[doc(hidden)]
    fn normalize_range_boundaries(&self, range: Range<usize>) -> (usize, usize) {
        let mut start = range.start;
        let mut end = range.end;

        for composite in self.composite_ranges() {
            let intersects = if range.is_empty() {
                composite.strictly_contains(range.start)
            } else {
                composite.range.start < end && start < composite.range.end
            };
            if intersects {
                start = start.min(composite.range.start);
                end = end.max(composite.range.end);
            }
        }

        (start, end)
    }
}

impl EditableText for String {
    fn cursor<'a>(&self, position: usize) -> Option<StringCursor<'_>> {
        let new_cursor = StringCursor {
            text: self,
            position,
        };

        if new_cursor.is_boundary() {
            Some(new_cursor)
        } else {
            None
        }
    }

    fn edit(&mut self, range: Range<usize>, new: impl Into<String>) {
        self.replace_range(range, &new.into());
    }

    fn slice(&self, range: Range<usize>) -> Option<Cow<'_, str>> {
        self.get(range).map(Cow::from)
    }

    fn len(&self) -> usize {
        self.len()
    }

    fn prev_grapheme_offset(&self, from: usize) -> Option<usize> {
        let mut c = GraphemeCursor::new(from, self.len(), true);
        c.prev_boundary(self, 0).unwrap()
    }

    fn next_grapheme_offset(&self, from: usize) -> Option<usize> {
        let mut c = GraphemeCursor::new(from, self.len(), true);
        c.next_boundary(self, 0).unwrap()
    }

    fn prev_codepoint_offset(&self, from: usize) -> Option<usize> {
        let mut c = self.cursor(from).unwrap();
        c.prev()
    }

    fn next_codepoint_offset(&self, from: usize) -> Option<usize> {
        let mut c = self.cursor(from).unwrap();
        if c.next().is_some() {
            Some(c.pos())
        } else {
            None
        }
    }

    fn prev_word_offset(&self, from: usize) -> Option<usize> {
        let mut offset = from;
        let mut passed_alphanumeric = false;
        for prev_grapheme in self.get(0..from)?.graphemes(true).rev() {
            let is_alphanumeric = prev_grapheme.chars().next()?.is_alphanumeric();
            if is_alphanumeric {
                passed_alphanumeric = true;
            } else if passed_alphanumeric {
                return Some(offset);
            }
            offset -= prev_grapheme.len();
        }
        None
    }

    fn next_word_offset(&self, from: usize) -> Option<usize> {
        let mut offset = from;
        let mut passed_alphanumeric = false;
        for next_grapheme in self.get(from..)?.graphemes(true) {
            let is_alphanumeric = next_grapheme.chars().next()?.is_alphanumeric();
            if is_alphanumeric {
                passed_alphanumeric = true;
            } else if passed_alphanumeric {
                return Some(offset);
            }
            offset += next_grapheme.len();
        }
        Some(self.len())
    }

    fn is_empty(&self) -> bool {
        self.is_empty()
    }

    fn from_str(s: &str) -> Self {
        s.to_string()
    }

    fn preceding_line_break(&self, from: usize) -> usize {
        let mut offset = from;

        for byte in self.get(0..from).unwrap_or("").bytes().rev() {
            if byte == 0x0a {
                return offset;
            }
            offset -= 1;
        }

        0
    }

    fn next_line_break(&self, from: usize) -> usize {
        let mut offset = from;

        for char in self.get(from..).unwrap_or("").bytes() {
            if char == 0x0a {
                return offset;
            }
            offset += 1;
        }

        self.len()
    }
}

impl EditableText for Arc<String> {
    fn cursor(&self, position: usize) -> Option<StringCursor<'_>> {
        <String as EditableText>::cursor(self, position)
    }
    fn edit(&mut self, range: Range<usize>, new: impl Into<String>) {
        let new = new.into();
        if !range.is_empty() || !new.is_empty() {
            Arc::make_mut(self).edit(range, new)
        }
    }
    fn slice(&self, range: Range<usize>) -> Option<Cow<'_, str>> {
        Some(Cow::Borrowed(&self[range]))
    }
    fn len(&self) -> usize {
        self.deref().len()
    }
    fn prev_word_offset(&self, offset: usize) -> Option<usize> {
        self.deref().prev_word_offset(offset)
    }
    fn next_word_offset(&self, offset: usize) -> Option<usize> {
        self.deref().next_word_offset(offset)
    }
    fn prev_grapheme_offset(&self, offset: usize) -> Option<usize> {
        self.deref().prev_grapheme_offset(offset)
    }
    fn next_grapheme_offset(&self, offset: usize) -> Option<usize> {
        self.deref().next_grapheme_offset(offset)
    }
    fn prev_codepoint_offset(&self, offset: usize) -> Option<usize> {
        self.deref().prev_codepoint_offset(offset)
    }
    fn next_codepoint_offset(&self, offset: usize) -> Option<usize> {
        self.deref().next_codepoint_offset(offset)
    }
    fn preceding_line_break(&self, offset: usize) -> usize {
        self.deref().preceding_line_break(offset)
    }
    fn next_line_break(&self, offset: usize) -> usize {
        self.deref().next_line_break(offset)
    }
    fn is_empty(&self) -> bool {
        self.deref().is_empty()
    }
    fn from_str(s: &str) -> Self {
        Arc::new(s.to_owned())
    }
}

/// A cursor with convenience functions for moving through EditableText.
pub trait EditableTextCursor<EditableText> {
    /// Set cursor position.
    fn set(&mut self, position: usize);

    /// Get cursor position.
    fn pos(&self) -> usize;

    /// Check if cursor position is at a codepoint boundary.
    fn is_boundary(&self) -> bool;

    /// Move cursor to previous codepoint boundary, if it exists.
    /// Returns previous codepoint as usize offset.
    fn prev(&mut self) -> Option<usize>;

    /// Move cursor to next codepoint boundary, if it exists.
    /// Returns current codepoint as usize offset.
    fn next(&mut self) -> Option<usize>;

    /// Get the next codepoint after the cursor position, without advancing
    /// the cursor.
    fn peek_next_codepoint(&self) -> Option<char>;

    /// Return codepoint preceding cursor offset and move cursor backward.
    fn prev_codepoint(&mut self) -> Option<char>;

    /// Return codepoint at cursor offset and move cursor forward.
    fn next_codepoint(&mut self) -> Option<char>;

    /// Return current offset if it's a boundary, else next.
    fn at_or_next(&mut self) -> Option<usize>;

    /// Return current offset if it's a boundary, else previous.
    fn at_or_prev(&mut self) -> Option<usize>;
}

/// A cursor type that implements EditableTextCursor for String
#[derive(Debug)]
pub struct StringCursor<'a> {
    text: &'a str,
    position: usize,
}

impl<'a> StringCursor<'a> {
    /// Create a new cursor.
    pub fn new(text: &'a str) -> Self {
        Self { text, position: 0 }
    }
}

impl<'a> EditableTextCursor<&'a String> for StringCursor<'a> {
    fn set(&mut self, position: usize) {
        self.position = position;
    }

    fn pos(&self) -> usize {
        self.position
    }

    fn is_boundary(&self) -> bool {
        self.text.is_char_boundary(self.position)
    }

    fn prev(&mut self) -> Option<usize> {
        let current_pos = self.pos();

        if current_pos == 0 {
            None
        } else {
            let mut len = 1;
            while !self.text.is_char_boundary(current_pos - len) {
                len += 1;
            }
            self.set(self.pos() - len);
            Some(self.pos())
        }
    }

    fn next(&mut self) -> Option<usize> {
        let current_pos = self.pos();

        if current_pos == self.text.len() {
            None
        } else {
            let b = self.text.as_bytes()[current_pos];
            self.set(current_pos + len_utf8_from_first_byte(b));
            Some(current_pos)
        }
    }

    fn peek_next_codepoint(&self) -> Option<char> {
        self.text[self.pos()..].chars().next()
    }

    fn prev_codepoint(&mut self) -> Option<char> {
        if let Some(prev) = self.prev() {
            self.text[prev..].chars().next()
        } else {
            None
        }
    }

    fn next_codepoint(&mut self) -> Option<char> {
        let current_index = self.pos();
        if self.next().is_some() {
            self.text[current_index..].chars().next()
        } else {
            None
        }
    }

    fn at_or_next(&mut self) -> Option<usize> {
        if self.is_boundary() {
            Some(self.pos())
        } else {
            self.next()
        }
    }

    fn at_or_prev(&mut self) -> Option<usize> {
        if self.is_boundary() {
            Some(self.pos())
        } else {
            self.prev()
        }
    }
}

pub fn len_utf8_from_first_byte(b: u8) -> usize {
    match b {
        b if b < 0x80 => 1,
        b if b < 0xe0 => 2,
        b if b < 0xf0 => 3,
        _ => 4,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Data;
    use test_log::test;

    #[test]
    fn replace() {
        let mut a = String::from("hello world");
        a.edit(1..9, "era");
        assert_eq!("herald", a);
    }

    #[test]
    fn prev_codepoint_offset() {
        let a = String::from("a\u{00A1}\u{4E00}\u{1F4A9}");
        assert_eq!(Some(6), a.prev_codepoint_offset(10));
        assert_eq!(Some(3), a.prev_codepoint_offset(6));
        assert_eq!(Some(1), a.prev_codepoint_offset(3));
        assert_eq!(Some(0), a.prev_codepoint_offset(1));
        assert_eq!(None, a.prev_codepoint_offset(0));
        let b = a.slice(1..10).unwrap().to_string();
        assert_eq!(Some(5), b.prev_codepoint_offset(9));
        assert_eq!(Some(2), b.prev_codepoint_offset(5));
        assert_eq!(Some(0), b.prev_codepoint_offset(2));
        assert_eq!(None, b.prev_codepoint_offset(0));
    }

    #[test]
    fn next_codepoint_offset() {
        let a = String::from("a\u{00A1}\u{4E00}\u{1F4A9}");
        assert_eq!(Some(10), a.next_codepoint_offset(6));
        assert_eq!(Some(6), a.next_codepoint_offset(3));
        assert_eq!(Some(3), a.next_codepoint_offset(1));
        assert_eq!(Some(1), a.next_codepoint_offset(0));
        assert_eq!(None, a.next_codepoint_offset(10));
        let b = a.slice(1..10).unwrap().to_string();
        assert_eq!(Some(9), b.next_codepoint_offset(5));
        assert_eq!(Some(5), b.next_codepoint_offset(2));
        assert_eq!(Some(2), b.next_codepoint_offset(0));
        assert_eq!(None, b.next_codepoint_offset(9));
    }

    #[test]
    fn prev_next() {
        let input = String::from("abc");
        let mut cursor = input.cursor(0).unwrap();
        assert_eq!(cursor.next(), Some(0));
        assert_eq!(cursor.next(), Some(1));
        assert_eq!(cursor.prev(), Some(1));
        assert_eq!(cursor.next(), Some(1));
        assert_eq!(cursor.next(), Some(2));
    }

    #[test]
    fn peek_next_codepoint() {
        let inp = String::from("$¢€£💶");
        let mut cursor = inp.cursor(0).unwrap();
        assert_eq!(cursor.peek_next_codepoint(), Some('$'));
        assert_eq!(cursor.peek_next_codepoint(), Some('$'));
        assert_eq!(cursor.next_codepoint(), Some('$'));
        assert_eq!(cursor.peek_next_codepoint(), Some('¢'));
        assert_eq!(cursor.prev_codepoint(), Some('$'));
        assert_eq!(cursor.peek_next_codepoint(), Some('$'));
        assert_eq!(cursor.next_codepoint(), Some('$'));
        assert_eq!(cursor.next_codepoint(), Some('¢'));
        assert_eq!(cursor.peek_next_codepoint(), Some('€'));
        assert_eq!(cursor.next_codepoint(), Some('€'));
        assert_eq!(cursor.peek_next_codepoint(), Some('£'));
        assert_eq!(cursor.next_codepoint(), Some('£'));
        assert_eq!(cursor.peek_next_codepoint(), Some('💶'));
        assert_eq!(cursor.next_codepoint(), Some('💶'));
        assert_eq!(cursor.peek_next_codepoint(), None);
        assert_eq!(cursor.next_codepoint(), None);
        assert_eq!(cursor.peek_next_codepoint(), None);
    }

    #[test]
    fn prev_grapheme_offset() {
        // A with ring, hangul, regional indicator "US"
        let a = String::from("A\u{030a}\u{110b}\u{1161}\u{1f1fa}\u{1f1f8}");
        assert_eq!(Some(9), a.prev_grapheme_offset(17));
        assert_eq!(Some(3), a.prev_grapheme_offset(9));
        assert_eq!(Some(0), a.prev_grapheme_offset(3));
        assert_eq!(None, a.prev_grapheme_offset(0));
    }

    #[test]
    fn next_grapheme_offset() {
        // A with ring, hangul, regional indicator "US"
        let a = String::from("A\u{030a}\u{110b}\u{1161}\u{1f1fa}\u{1f1f8}");
        assert_eq!(Some(3), a.next_grapheme_offset(0));
        assert_eq!(Some(9), a.next_grapheme_offset(3));
        assert_eq!(Some(17), a.next_grapheme_offset(9));
        assert_eq!(None, a.next_grapheme_offset(17));
    }

    #[test]
    fn prev_word_offset() {
        let a = String::from("Technically a word: ৬藏A\u{030a}\u{110b}\u{1161}");
        assert_eq!(Some(20), a.prev_word_offset(35));
        assert_eq!(Some(20), a.prev_word_offset(27));
        assert_eq!(Some(20), a.prev_word_offset(23));
        assert_eq!(Some(14), a.prev_word_offset(20));
        assert_eq!(Some(14), a.prev_word_offset(19));
        assert_eq!(Some(12), a.prev_word_offset(13));
        assert_eq!(None, a.prev_word_offset(12));
        assert_eq!(None, a.prev_word_offset(11));
        assert_eq!(None, a.prev_word_offset(0));
    }

    #[test]
    fn next_word_offset() {
        let a = String::from("Technically a word: ৬藏A\u{030a}\u{110b}\u{1161}");
        assert_eq!(Some(11), a.next_word_offset(0));
        assert_eq!(Some(11), a.next_word_offset(7));
        assert_eq!(Some(13), a.next_word_offset(11));
        assert_eq!(Some(18), a.next_word_offset(14));
        assert_eq!(Some(35), a.next_word_offset(18));
        assert_eq!(Some(35), a.next_word_offset(19));
        assert_eq!(Some(35), a.next_word_offset(20));
        assert_eq!(Some(35), a.next_word_offset(26));
        assert_eq!(Some(35), a.next_word_offset(35));
    }

    #[test]
    fn preceding_line_break() {
        let a = String::from("Technically\na word:\n ৬藏A\u{030a}\n\u{110b}\u{1161}");
        assert_eq!(0, a.preceding_line_break(0));
        assert_eq!(0, a.preceding_line_break(11));
        assert_eq!(12, a.preceding_line_break(12));
        assert_eq!(12, a.preceding_line_break(13));
        assert_eq!(20, a.preceding_line_break(21));
        assert_eq!(31, a.preceding_line_break(31));
        assert_eq!(31, a.preceding_line_break(34));

        let b = String::from("Technically a word: ৬藏A\u{030a}\u{110b}\u{1161}");
        assert_eq!(0, b.preceding_line_break(0));
        assert_eq!(0, b.preceding_line_break(11));
        assert_eq!(0, b.preceding_line_break(13));
        assert_eq!(0, b.preceding_line_break(21));
    }

    #[test]
    fn next_line_break() {
        let a = String::from("Technically\na word:\n ৬藏A\u{030a}\n\u{110b}\u{1161}");
        assert_eq!(11, a.next_line_break(0));
        assert_eq!(11, a.next_line_break(11));
        assert_eq!(19, a.next_line_break(13));
        assert_eq!(30, a.next_line_break(21));
        assert_eq!(a.len(), a.next_line_break(31));

        let b = String::from("Technically a word: ৬藏A\u{030a}\u{110b}\u{1161}");
        assert_eq!(b.len(), b.next_line_break(0));
        assert_eq!(b.len(), b.next_line_break(11));
        assert_eq!(b.len(), b.next_line_break(13));
        assert_eq!(b.len(), b.next_line_break(19));
    }

    #[test]
    fn arcstring_empty_edit() {
        let a = Arc::new("hello".to_owned());
        let mut b = a.clone();
        b.edit(5..5, "");
        assert!(a.same(&b));
    }

    struct CompositeText {
        text: String,
        composites: Vec<CompositeTextRange>,
    }

    impl CompositeText {
        fn new(text: &str, composites: Vec<CompositeTextRange>) -> Self {
            Self {
                text: text.to_owned(),
                composites,
            }
        }
    }

    impl EditableText for CompositeText {
        fn cursor(&self, position: usize) -> Option<StringCursor<'_>> {
            self.text.cursor(position)
        }

        fn edit(&mut self, range: Range<usize>, new: impl Into<String>) {
            self.text.edit(range, new);
        }

        fn slice(&self, range: Range<usize>) -> Option<Cow<'_, str>> {
            self.text.slice(range)
        }

        fn len(&self) -> usize {
            self.text.len()
        }

        fn prev_word_offset(&self, offset: usize) -> Option<usize> {
            self.text.prev_word_offset(offset)
        }

        fn next_word_offset(&self, offset: usize) -> Option<usize> {
            self.text.next_word_offset(offset)
        }

        fn prev_grapheme_offset(&self, offset: usize) -> Option<usize> {
            self.text.prev_grapheme_offset(offset)
        }

        fn next_grapheme_offset(&self, offset: usize) -> Option<usize> {
            self.text.next_grapheme_offset(offset)
        }

        fn prev_codepoint_offset(&self, offset: usize) -> Option<usize> {
            self.text.prev_codepoint_offset(offset)
        }

        fn next_codepoint_offset(&self, offset: usize) -> Option<usize> {
            self.text.next_codepoint_offset(offset)
        }

        fn preceding_line_break(&self, offset: usize) -> usize {
            self.text.preceding_line_break(offset)
        }

        fn next_line_break(&self, offset: usize) -> usize {
            self.text.next_line_break(offset)
        }

        fn is_empty(&self) -> bool {
            self.text.is_empty()
        }

        fn from_str(s: &str) -> Self {
            Self::new(s, Vec::new())
        }

        fn composite_ranges(&self) -> &[CompositeTextRange] {
            &self.composites
        }

        fn composite_text(&self, _composite: &CompositeTextRange) -> Cow<'_, str> {
            Cow::Borrowed("[reference]")
        }
    }

    fn composite_document() -> CompositeText {
        CompositeText::new(
            "ab<label>cd",
            vec![CompositeTextRange::new(CompositeKey::new(7), 2..9)],
        )
    }

    #[test]
    fn composite_lookup_and_caret_normalization() {
        let text = composite_document();
        assert_eq!(
            Some(CompositeTextRange::new(CompositeKey::new(7), 2..9)),
            text.composite_at(4)
        );
        assert_eq!(text.normalize_caret_offset(4, TextBoundary::Upstream), 2);
        assert_eq!(text.normalize_caret_offset(4, TextBoundary::Downstream), 9);
        assert_eq!(text.normalize_caret_offset(2, TextBoundary::Upstream), 2);
        assert_eq!(text.normalize_caret_offset(2, TextBoundary::Downstream), 2);
        assert_eq!(text.normalize_caret_offset(9, TextBoundary::Upstream), 9);
    }

    #[test]
    fn composite_selection_and_edit_normalization() {
        let text = composite_document();
        assert_eq!(
            text.normalize_selection(Selection::caret(4)),
            Selection::new(2, 9)
        );
        assert_eq!(
            text.normalize_selection(Selection::new(4, 6)),
            Selection::new(2, 9)
        );
        assert_eq!(
            text.normalize_selection(Selection::caret(2)),
            Selection::caret(2)
        );
        assert_eq!(
            text.normalize_selection(Selection::caret(9)),
            Selection::caret(9)
        );
        assert_eq!(text.normalize_edit_range(4..6), 2..9);
        assert_eq!(text.normalize_edit_range(0..4), 0..9);
        assert_eq!(text.normalize_edit_range(2..2), 2..2);
        assert_eq!(text.normalize_edit_range(9..9), 9..9);
        assert_eq!(text.normalize_edit_range(0..2), 0..2);
        assert_eq!(text.normalize_edit_range(9..11), 9..11);
    }

    #[test]
    fn composite_clipboard_serialization() {
        let text = composite_document();
        assert_eq!(text.clipboard_text(0..11), "ab[reference]cd");
        assert_eq!(text.clipboard_text(2..9), "[reference]");
    }

    #[test]
    fn composite_validation_rejects_invalid_ranges() {
        assert_eq!(composite_document().validate_composite_ranges(), Ok(()));
        assert_eq!(
            CompositeText::new(
                "abc",
                vec![CompositeTextRange::new(CompositeKey::new(1), 2..2)]
            )
            .validate_composite_ranges(),
            Err(CompositeTextRangeError::EmptyRange)
        );
        assert_eq!(
            CompositeText::new(
                "abcdef",
                vec![
                    CompositeTextRange::new(CompositeKey::new(1), 1..4),
                    CompositeTextRange::new(CompositeKey::new(2), 3..5),
                ],
            )
            .validate_composite_ranges(),
            Err(CompositeTextRangeError::UnsortedOrOverlapping)
        );
    }
}
