//! Ordered observations shared by tools and the conversation record.

use serde::{Deserialize, Serialize};

use crate::image::ImageAsset;

/// Persisted content contains verified assets. Request preparation maps the
/// image type to provider inputs without changing observation order.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Content<I = ImageAsset> {
    pub blocks: Vec<ContentBlock<I>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContentBlock<I = ImageAsset> {
    Text { text: String },
    Image { image: I },
}

impl<I> Default for Content<I> {
    fn default() -> Self {
        Self { blocks: Vec::new() }
    }
}

impl<I> Content<I> {
    #[must_use]
    pub fn new(blocks: Vec<ContentBlock<I>>) -> Self {
        Self { blocks }
    }

    #[must_use]
    pub fn from_text(text: impl Into<String>) -> Self {
        Self::new(vec![ContentBlock::Text { text: text.into() }])
    }

    pub fn text_blocks(&self) -> impl Iterator<Item = &str> {
        self.blocks.iter().filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            ContentBlock::Image { .. } => None,
        })
    }

    /// Text-only projection for parsers and failure classification.
    #[must_use]
    pub fn text(&self) -> String {
        self.text_blocks().collect()
    }

    pub fn images(&self) -> impl Iterator<Item = &I> {
        self.blocks.iter().filter_map(|block| match block {
            ContentBlock::Image { image } => Some(image),
            ContentBlock::Text { .. } => None,
        })
    }

    #[must_use]
    pub fn has_images(&self) -> bool {
        self.images().next().is_some()
    }

    #[must_use]
    pub fn image_count(&self) -> usize {
        self.images().count()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.blocks
            .iter()
            .all(|block| matches!(block, ContentBlock::Text { text } if text.is_empty()))
    }

    pub fn push_text(&mut self, text: impl AsRef<str>) {
        let text = text.as_ref();
        if let Some(ContentBlock::Text { text: previous }) = self.blocks.last_mut() {
            previous.push_str(text);
        } else {
            self.blocks.push(ContentBlock::Text {
                text: text.to_owned(),
            });
        }
    }

    pub fn push_image(&mut self, image: I) {
        self.blocks.push(ContentBlock::Image { image });
    }

    #[must_use]
    pub fn map_images<J>(&self, mut map: impl FnMut(&I) -> J) -> Content<J> {
        Content::new(
            self.blocks
                .iter()
                .map(|block| match block {
                    ContentBlock::Text { text } => ContentBlock::Text { text: text.clone() },
                    ContentBlock::Image { image } => ContentBlock::Image { image: map(image) },
                })
                .collect(),
        )
    }
}

impl<I: std::fmt::Display> Content<I> {
    /// Transcript projection retaining image identity, without claiming to
    /// describe pixels that a text consumer has not observed.
    #[must_use]
    pub fn summary_text(&self) -> String {
        let mut output = String::new();
        for block in &self.blocks {
            match block {
                ContentBlock::Text { text } => output.push_str(text),
                ContentBlock::Image { image } => {
                    if !output.is_empty() && !output.ends_with('\n') {
                        output.push('\n');
                    }
                    output.push_str(&format!("[image: {image}]\n"));
                }
            }
        }
        output
    }
}

impl<I: std::fmt::Display> std::fmt::Display for Content<I> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.summary_text())
    }
}

impl<I> From<String> for Content<I> {
    fn from(text: String) -> Self {
        Self::from_text(text)
    }
}

impl<I> From<&str> for Content<I> {
    fn from(text: &str) -> Self {
        Self::from_text(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_mapping_preserves_observation_order() {
        let content = Content::new(vec![
            ContentBlock::Text {
                text: "before".into(),
            },
            ContentBlock::Image { image: 7 },
            ContentBlock::Text {
                text: "after".into(),
            },
        ]);
        let mapped = content.map_images(|image| image.to_string());
        assert!(matches!(&mapped.blocks[1], ContentBlock::Image { image } if image == "7"));
        assert_eq!(mapped.text(), "beforeafter");
        assert!(mapped.summary_text().contains("[image: 7]"));
    }
}
